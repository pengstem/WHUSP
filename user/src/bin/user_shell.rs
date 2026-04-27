#![no_std]
#![no_main]
#![allow(clippy::println_empty_string)]

extern crate alloc;

#[macro_use]
extern crate user_lib;

const LF: u8 = 0x0au8;
const CR: u8 = 0x0du8;
const DL: u8 = 0x7fu8;
const BS: u8 = 0x08u8;
const LINE_START: &str = ">> ";

use alloc::string::String;
use alloc::vec::Vec;
use user_lib::console::getchar;
use user_lib::{OpenFlags, chdir, close, dup, exec, fork, getcwd, getdents64, open, pipe, waitpid};

#[derive(Debug)]
struct ProcessArguments {
    input: String,
    output: String,
    args_copy: Vec<String>,
    args_addr: Vec<*const u8>,
}

impl ProcessArguments {
    pub fn new(command: &str) -> Self {
        let args: Vec<_> = command.split(' ').collect();
        let mut args_copy: Vec<String> = args
            .iter()
            .filter(|&arg| !arg.is_empty())
            .map(|&arg| {
                let mut string = String::new();
                string.push_str(arg);
                string.push('\0');
                string
            })
            .collect();

        // redirect input
        let mut input = String::new();
        if let Some((idx, _)) = args_copy
            .iter()
            .enumerate()
            .find(|(_, arg)| arg.as_str() == "<\0")
        {
            input = args_copy[idx + 1].clone();
            args_copy.drain(idx..=idx + 1);
        }

        // redirect output
        let mut output = String::new();
        if let Some((idx, _)) = args_copy
            .iter()
            .enumerate()
            .find(|(_, arg)| arg.as_str() == ">\0")
        {
            output = args_copy[idx + 1].clone();
            args_copy.drain(idx..=idx + 1);
        }

        let mut args_addr: Vec<*const u8> = args_copy.iter().map(|arg| arg.as_ptr()).collect();
        args_addr.push(core::ptr::null::<u8>());

        Self {
            input,
            output,
            args_copy,
            args_addr,
        }
    }
}

fn try_builtin(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let Some(cmd) = parts.next() else {
        return false;
    };
    match cmd {
        "pwd" => {
            let mut buf = [0u8; 256];
            let ret = getcwd(&mut buf);
            if ret == 0 {
                println!("pwd: failed");
            } else {
                let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                match core::str::from_utf8(&buf[..len]) {
                    Ok(s) => println!("{}", s),
                    Err(_) => println!("pwd: invalid utf8"),
                }
            }
            true
        }
        "cd" => {
            let target = parts.next().unwrap_or("/");
            let mut path = String::from(target);
            path.push('\0');
            if chdir(path.as_str()) < 0 {
                println!("cd: {}: no such directory", target);
            }
            true
        }
        "ls" => {
            let target = parts.next().unwrap_or(".");
            let mut path = String::from(target);
            path.push('\0');
            let fd = open(path.as_str(), OpenFlags::RDONLY | OpenFlags::DIRECTORY);
            if fd < 0 {
                println!("ls: cannot access '{}'", target);
            } else {
                let fd = fd as usize;
                let mut buf = [0u8; 1024];
                loop {
                    let n = getdents64(fd, &mut buf);
                    if n <= 0 {
                        break;
                    }
                    let n = n as usize;
                    let mut off = 0usize;
                    while off + 19 <= n {
                        let reclen = u16::from_ne_bytes([buf[off + 16], buf[off + 17]]) as usize;
                        if reclen == 0 || off + reclen > n {
                            break;
                        }
                        let name_bytes = &buf[off + 19..off + reclen];
                        let name_end = name_bytes
                            .iter()
                            .position(|&b| b == 0)
                            .unwrap_or(name_bytes.len());
                        if let Ok(s) = core::str::from_utf8(&name_bytes[..name_end]) {
                            print!("{}  ", s);
                        }
                        off += reclen;
                    }
                }
                println!("");
                close(fd);
            }
            true
        }
        _ => false,
    }
}

fn print_prompt() {
    let mut buf = [0u8; 256];
    if getcwd(&mut buf) != 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = core::str::from_utf8(&buf[..len]) {
            print!("{} {}", s, LINE_START);
            return;
        }
    }
    print!("{}", LINE_START);
}

#[unsafe(no_mangle)]
pub fn main() -> i32 {
    println!("Rust user shell");
    let mut line: String = String::new();
    print_prompt();
    loop {
        let c = getchar();
        match c {
            LF | CR => {
                println!("");
                if !line.is_empty() {
                    if try_builtin(line.as_str()) {
                        line.clear();
                        print_prompt();
                        continue;
                    }
                    let splited: Vec<_> = line.as_str().split('|').collect();
                    let process_arguments_list: Vec<_> = splited
                        .iter()
                        .map(|&cmd| ProcessArguments::new(cmd))
                        .collect();
                    let mut valid = true;
                    for (i, process_args) in process_arguments_list.iter().enumerate() {
                        if i == 0 {
                            if !process_args.output.is_empty() {
                                valid = false;
                            }
                        } else if i == process_arguments_list.len() - 1 {
                            if !process_args.input.is_empty() {
                                valid = false;
                            }
                        } else if !process_args.output.is_empty() || !process_args.input.is_empty()
                        {
                            valid = false;
                        }
                    }
                    if process_arguments_list.len() == 1 {
                        valid = true;
                    }
                    if !valid {
                        println!("Invalid command: Inputs/Outputs cannot be correctly binded!");
                    } else {
                        // create pipes
                        let mut pipes_fd: Vec<[i32; 2]> = Vec::new();
                        if !process_arguments_list.is_empty() {
                            for _ in 0..process_arguments_list.len() - 1 {
                                let mut pipe_fd = [0i32; 2];
                                pipe(&mut pipe_fd);
                                pipes_fd.push(pipe_fd);
                            }
                        }
                        let mut children: Vec<_> = Vec::new();
                        for (i, process_argument) in process_arguments_list.iter().enumerate() {
                            let pid = fork();
                            if pid == 0 {
                                let input = &process_argument.input;
                                let output = &process_argument.output;
                                let args_copy = &process_argument.args_copy;
                                let args_addr = &process_argument.args_addr;
                                // redirect input
                                if !input.is_empty() {
                                    let input_fd = open(input.as_str(), OpenFlags::RDONLY);
                                    if input_fd == -1 {
                                        println!("Error when opening file {}", input);
                                        return -4;
                                    }
                                    let input_fd = input_fd as usize;
                                    close(0);
                                    assert_eq!(dup(input_fd), 0);
                                    close(input_fd);
                                }
                                // redirect output
                                if !output.is_empty() {
                                    let output_fd = open(
                                        output.as_str(),
                                        OpenFlags::CREATE | OpenFlags::WRONLY,
                                    );
                                    if output_fd == -1 {
                                        println!("Error when opening file {}", output);
                                        return -4;
                                    }
                                    let output_fd = output_fd as usize;
                                    close(1);
                                    assert_eq!(dup(output_fd), 1);
                                    close(output_fd);
                                }
                                // receive input from the previous process
                                if i > 0 {
                                    close(0);
                                    let read_end = pipes_fd.get(i - 1).unwrap()[0] as usize;
                                    assert_eq!(dup(read_end), 0);
                                }
                                // send output to the next process
                                if i < process_arguments_list.len() - 1 {
                                    close(1);
                                    let write_end = pipes_fd.get(i).unwrap()[1] as usize;
                                    assert_eq!(dup(write_end), 1);
                                }
                                // close all pipe ends inherited from the parent process
                                for pipe_fd in pipes_fd.iter() {
                                    close(pipe_fd[0] as usize);
                                    close(pipe_fd[1] as usize);
                                }
                                // execute new application
                                if exec(args_copy[0].as_str(), args_addr.as_slice()) == -1 {
                                    println!("Error when executing!");
                                    return -4;
                                }
                                unreachable!();
                            } else {
                                children.push(pid);
                            }
                        }
                        for pipe_fd in pipes_fd.iter() {
                            close(pipe_fd[0] as usize);
                            close(pipe_fd[1] as usize);
                        }
                        let mut exit_code: i32 = 0;
                        for pid in children.into_iter() {
                            let exit_pid = waitpid(pid as usize, &mut exit_code);
                            assert_eq!(pid, exit_pid);
                            //println!("Shell: Process {} exited with code {}", pid, exit_code);
                        }
                    }
                    line.clear();
                }
                print_prompt();
            }
            BS | DL => {
                if !line.is_empty() {
                    print!("{}", BS as char);
                    print!(" ");
                    print!("{}", BS as char);
                    line.pop();
                }
            }
            _ => {
                print!("{}", c as char);
                line.push(c as char);
            }
        }
    }
}
