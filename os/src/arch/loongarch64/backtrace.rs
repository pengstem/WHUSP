use core::arch::asm;

pub fn frame_pointer() -> usize {
    let fp;
    unsafe {
        asm!("move {fp}, $fp", fp = out(reg) fp);
    }
    fp
}
