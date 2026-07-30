/// Linux native-width scatter/gather vector.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LinuxIovec {
    pub(crate) base: usize,
    pub(crate) len: usize,
}

const _: () = {
    assert!(core::mem::size_of::<LinuxIovec>() == 2 * core::mem::size_of::<usize>());
    assert!(core::mem::align_of::<LinuxIovec>() == core::mem::align_of::<usize>());
};
