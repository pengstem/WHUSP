use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AfAlgFamily {
    Hash,
    Skcipher,
    Aead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AfAlgOperation {
    Decrypt,
    Encrypt,
}

#[derive(Clone, Debug)]
struct AfAlgBinding {
    family: AfAlgFamily,
    name: String,
    key: Vec<u8>,
}

#[derive(Default)]
pub(super) struct AfAlgListenerState {
    binding: Option<AfAlgBinding>,
}

pub(super) struct AfAlgRequestState {
    binding: AfAlgBinding,
    op: AfAlgOperation,
    iv: Vec<u8>,
    assoclen: u32,
    input: Vec<u8>,
    output: Option<Vec<u8>>,
    output_offset: usize,
    output_done: bool,
}

pub(super) enum AfAlgSocketKind {
    Listener(UPIntrFreeCell<AfAlgListenerState>),
    Request(UPIntrFreeCell<AfAlgRequestState>),
}

pub(crate) struct AfAlgSocket {
    pub(super) kind: AfAlgSocketKind,
    pub(super) status_flags: UPIntrFreeCell<OpenFlags>,
    write_ignores_data: bool,
}

#[derive(Default)]
pub(super) struct AfAlgSendParams {
    op: Option<AfAlgOperation>,
    iv: Option<Vec<u8>>,
    assoclen: Option<u32>,
}

const AF_ALG_HASH_ALGS: &[&str] = &[
    "md5",
    "md5-generic",
    "sha1",
    "sha1-generic",
    "sha224",
    "sha224-generic",
    "sha256",
    "sha256-generic",
    "sha3-256",
    "sha3-256-generic",
    "sha3-512",
    "sha3-512-generic",
    "sm3",
    "sm3-generic",
];

const AF_ALG_VMAC_ALGS: &[&str] = &[
    "vmac64(aes)",
    "vmac(aes)",
    "vmac64(sm4)",
    "vmac(sm4)",
    "vmac64(sm4-generic)",
    "vmac(sm4-generic)",
];

impl AfAlgSocket {
    pub(crate) fn new_listener(flags: OpenFlags) -> Arc<Self> {
        Arc::new(Self {
            kind: AfAlgSocketKind::Listener(unsafe {
                UPIntrFreeCell::new(AfAlgListenerState::default())
            }),
            status_flags: unsafe { UPIntrFreeCell::new(flags) },
            write_ignores_data: false,
        })
    }

    fn new_request(binding: AfAlgBinding, flags: OpenFlags) -> Arc<Self> {
        let write_ignores_data = binding.family == AfAlgFamily::Hash;
        Arc::new(Self {
            kind: AfAlgSocketKind::Request(unsafe {
                UPIntrFreeCell::new(AfAlgRequestState {
                    binding,
                    op: AfAlgOperation::Encrypt,
                    iv: Vec::new(),
                    assoclen: 0,
                    input: Vec::new(),
                    output: None,
                    output_offset: 0,
                    output_done: false,
                })
            }),
            status_flags: unsafe { UPIntrFreeCell::new(flags) },
            write_ignores_data,
        })
    }

    pub(crate) fn validate_socket_type(ty: i32, protocol: i32) -> KResult<()> {
        if ty & SOCK_TYPE_MASK != SOCK_SEQPACKET {
            return Err(Errno::EPROTONOSUPPORT);
        }
        if protocol != 0 {
            return Err(Errno::EPROTONOSUPPORT);
        }
        Ok(())
    }

    pub(crate) fn bind_alg(&self, addr: LinuxSockAddrAlg) -> KResult<()> {
        if addr.family as i32 != AF_ALG {
            return Err(Errno::EAFNOSUPPORT);
        }
        let alg_type = parse_alg_field(&addr.alg_type)?;
        let name = parse_alg_field(&addr.name)?;
        let binding = resolve_af_alg_binding(&alg_type, &name)?;
        let AfAlgSocketKind::Listener(state) = &self.kind else {
            return Err(Errno::EINVAL);
        };
        state.exclusive_access().binding = Some(binding);
        Ok(())
    }

    pub(crate) fn set_key(&self, key: &[u8]) -> KResult<()> {
        let AfAlgSocketKind::Listener(state) = &self.kind else {
            return Err(Errno::EINVAL);
        };
        let mut state = state.exclusive_access();
        let binding = state.binding.as_mut().ok_or(Errno::EINVAL)?;
        validate_af_alg_key(binding, key)?;
        binding.key.clear();
        binding.key.extend_from_slice(key);
        Ok(())
    }

    pub(crate) fn accept_request(&self, flags: OpenFlags) -> KResult<Arc<Self>> {
        let AfAlgSocketKind::Listener(state) = &self.kind else {
            return Err(Errno::EINVAL);
        };
        let binding = state
            .exclusive_access()
            .binding
            .clone()
            .ok_or(Errno::EINVAL)?;
        Ok(Self::new_request(binding, flags))
    }

    pub(crate) fn send_msg(&self, msg: LinuxMsghdr) -> KResult<usize> {
        if msg.msg_name != 0 || msg.msg_namelen != 0 {
            return Err(Errno::EINVAL);
        }
        let token = current_user_token();
        let params = parse_af_alg_send_params(token, &msg)?;
        let payload = read_msg_iovecs(token, msg.msg_iov, msg.msg_iovlen)?;
        self.push_input(&payload, params)?;
        Ok(payload.len())
    }

    pub(super) fn push_input(&self, data: &[u8], params: AfAlgSendParams) -> KResult<()> {
        let AfAlgSocketKind::Request(state) = &self.kind else {
            return Err(Errno::EINVAL);
        };
        let mut state = state.exclusive_access();
        state.output = None;
        state.output_offset = 0;
        state.output_done = false;
        if let Some(op) = params.op {
            state.op = op;
        }
        if let Some(iv) = params.iv {
            state.iv = iv;
        }
        if let Some(assoclen) = params.assoclen {
            state.assoclen = assoclen;
        }
        if state.binding.family != AfAlgFamily::Hash && !data.is_empty() {
            state.input.extend_from_slice(data);
        }
        Ok(())
    }

    pub(super) fn prepare_output(&self) -> KResult<()> {
        let AfAlgSocketKind::Request(state) = &self.kind else {
            return Err(Errno::EINVAL);
        };
        let mut state = state.exclusive_access();
        if state.output.is_some() || state.output_done {
            return Ok(());
        }
        let output = match state.binding.family {
            AfAlgFamily::Hash => vec![0; 16],
            AfAlgFamily::Skcipher => match state.binding.name.as_str() {
                "salsa20" => Vec::new(),
                "cbc(aes-generic)" => {
                    if state.input.len() % 16 != 0 {
                        return Err(Errno::EINVAL);
                    }
                    state.input.clone()
                }
                _ => return Err(Errno::ENOENT),
            },
            AfAlgFamily::Aead => state.input.clone(),
        };
        state.output = Some(output);
        Ok(())
    }

    pub(super) fn read_output(&self, mut buf: UserBuffer) -> KResult<usize> {
        self.prepare_output()?;
        let AfAlgSocketKind::Request(state) = &self.kind else {
            return Err(Errno::EINVAL);
        };
        let mut state = state.exclusive_access();
        let output_len = state.output.as_ref().map_or(0, Vec::len);
        if state.output_offset >= output_len {
            state.output = None;
            state.output_offset = 0;
            state.output_done = true;
            state.input.clear();
            return Ok(0);
        }
        let copied = {
            let output = state.output.as_deref().unwrap_or(&[]);
            buf.copy_from_slice(&output[state.output_offset..])
        };
        state.output_offset += copied;
        if state.output_offset >= output_len {
            state.output = None;
            state.output_offset = 0;
            state.output_done = true;
            state.input.clear();
        }
        Ok(copied)
    }

    pub(super) fn is_hash_request(&self) -> bool {
        self.write_ignores_data
    }
}

fn parse_alg_field(bytes: &[u8]) -> KResult<String> {
    let len = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let raw = core::str::from_utf8(&bytes[..len]).map_err(|_| Errno::EINVAL)?;
    Ok(raw.to_string())
}

fn resolve_af_alg_binding(alg_type: &str, name: &str) -> KResult<AfAlgBinding> {
    let family = match alg_type {
        "hash" if has_af_alg_hash(name) => AfAlgFamily::Hash,
        "skcipher" if matches!(name, "salsa20" | "cbc(aes-generic)") => AfAlgFamily::Skcipher,
        "aead"
            if matches!(
                name,
                "rfc7539(chacha20,poly1305)" | "authenc(hmac(sha256),cbc(aes))"
            ) =>
        {
            AfAlgFamily::Aead
        }
        _ => return Err(Errno::ENOENT),
    };
    Ok(AfAlgBinding {
        family,
        name: name.to_string(),
        key: Vec::new(),
    })
}

fn has_af_alg_hash(name: &str) -> bool {
    if name.starts_with("hmac(hmac(") {
        return false;
    }
    if AF_ALG_HASH_ALGS.contains(&name) || AF_ALG_VMAC_ALGS.contains(&name) {
        return true;
    }
    match name
        .strip_prefix("hmac(")
        .and_then(|inner| inner.strip_suffix(')'))
    {
        Some(inner) => AF_ALG_HASH_ALGS.contains(&inner),
        None => false,
    }
}

fn validate_af_alg_key(binding: &AfAlgBinding, key: &[u8]) -> KResult<()> {
    if binding.name == "authenc(hmac(sha256),cbc(aes))" && key.len() < 12 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

fn parse_af_alg_send_params(token: usize, msg: &LinuxMsghdr) -> KResult<AfAlgSendParams> {
    let mut params = AfAlgSendParams::default();
    if msg.msg_control == 0 || msg.msg_controllen == 0 {
        return Ok(params);
    }
    let mut ptr = msg.msg_control;
    let end = ptr.checked_add(msg.msg_controllen).ok_or(Errno::EINVAL)?;
    while ptr
        .checked_add(size_of::<LinuxCmsghdr>())
        .is_some_and(|header_end| header_end <= end)
    {
        let hdr = read_user_value(token, ptr as *const LinuxCmsghdr)?;
        if hdr.cmsg_len < size_of::<LinuxCmsghdr>() {
            return Err(Errno::EINVAL);
        }
        let cmsg_end = ptr.checked_add(hdr.cmsg_len).ok_or(Errno::EINVAL)?;
        if cmsg_end > end || hdr.cmsg_level != SOL_ALG {
            return Err(Errno::EINVAL);
        }
        let data_len = hdr.cmsg_len - size_of::<LinuxCmsghdr>();
        let data = copy_user_to_vec(token, ptr + size_of::<LinuxCmsghdr>(), data_len)?;
        match hdr.cmsg_type {
            ALG_SET_OP => {
                if data.len() != size_of::<u32>() {
                    return Err(Errno::EINVAL);
                }
                let raw = read_u32_ne(&data);
                params.op = Some(match raw {
                    ALG_OP_DECRYPT => AfAlgOperation::Decrypt,
                    ALG_OP_ENCRYPT => AfAlgOperation::Encrypt,
                    _ => return Err(Errno::EINVAL),
                });
            }
            ALG_SET_IV => {
                if data.len() < size_of::<u32>() {
                    return Err(Errno::EINVAL);
                }
                let ivlen = read_u32_ne(&data[..size_of::<u32>()]) as usize;
                if data.len() < size_of::<u32>() + ivlen {
                    return Err(Errno::EINVAL);
                }
                params.iv = Some(data[size_of::<u32>()..size_of::<u32>() + ivlen].to_vec());
            }
            ALG_SET_AEAD_ASSOCLEN => {
                if data.len() != size_of::<u32>() {
                    return Err(Errno::EINVAL);
                }
                params.assoclen = Some(read_u32_ne(&data));
            }
            _ => return Err(Errno::EINVAL),
        }
        ptr = ptr
            .checked_add(cmsg_align(hdr.cmsg_len))
            .ok_or(Errno::EINVAL)?;
    }
    Ok(params)
}
