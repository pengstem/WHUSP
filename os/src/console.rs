use crate::drivers::chardev::UART;
use core::fmt;

pub fn print(args: fmt::Arguments) {
    UART.write_fmt_record(args).unwrap();
}

pub fn emergency_print(args: fmt::Arguments) {
    UART.emergency_write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($fmt: literal $(, $($arg: tt)+)?) => {
        $crate::console::print(format_args!($fmt $(, $($arg)+)?))
    }
}

#[macro_export]
macro_rules! println {
    ($fmt: literal $(, $($arg: tt)+)?) => {
        $crate::console::print(format_args!(concat!($fmt, "\n") $(, $($arg)+)?))
    }
}
