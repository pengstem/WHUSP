use crate::sync::UPIntrFreeCell;
use crate::task::{current_process, current_task, current_user_token};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

use super::errno::{SysError, SysResult};
use super::user_ptr::{
    PATH_MAX, UserBufferAccess, read_user_c_string, translated_byte_buffer_checked_with_mmap_fault,
};

const KEY_SPEC_THREAD_KEYRING: i32 = -1;
const KEY_SPEC_PROCESS_KEYRING: i32 = -2;
const KEY_SPEC_SESSION_KEYRING: i32 = -3;
const KEY_SPEC_USER_KEYRING: i32 = -4;
const KEY_SPEC_USER_SESSION_KEYRING: i32 = -5;

const KEYCTL_GET_KEYRING_ID: usize = 0;
const KEYCTL_JOIN_SESSION_KEYRING: usize = 1;

const USER_KEY_MAX_PAYLOAD: usize = 32_767;
const BIG_KEY_MAX_PAYLOAD: usize = (1 << 20) - 1;
const DEFAULT_KEY_GC_DELAY: usize = 300;
const DEFAULT_KEY_MAXKEYS: usize = 200;
const DEFAULT_KEY_MAXBYTES: usize = 20_000;

static KEY_GC_DELAY: AtomicUsize = AtomicUsize::new(DEFAULT_KEY_GC_DELAY);
static KEY_MAXKEYS: AtomicUsize = AtomicUsize::new(DEFAULT_KEY_MAXKEYS);
static KEY_MAXBYTES: AtomicUsize = AtomicUsize::new(DEFAULT_KEY_MAXBYTES);

lazy_static! {
    static ref KEY_MANAGER: UPIntrFreeCell<KeyManager> =
        unsafe { UPIntrFreeCell::new(KeyManager::new()) };
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum KeyKind {
    Keyring,
    User,
    Logon,
    BigKey,
    ParserOnly,
}

impl KeyKind {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "keyring" => Some(Self::Keyring),
            "user" => Some(Self::User),
            "logon" => Some(Self::Logon),
            "big_key" => Some(Self::BigKey),
            "asymmetric" | "cifs.idmap" | "cifs.spnego" | "pkcs7_test" | "rxrpc" | "rxrpc_s" => {
                Some(Self::ParserOnly)
            }
            _ => None,
        }
    }

    fn can_store(self) -> bool {
        !matches!(self, Self::ParserOnly)
    }

    fn validate_payload_len(self, description: &str, len: usize) -> SysResult<()> {
        match self {
            Self::Keyring => {
                if len == 0 {
                    Ok(())
                } else {
                    Err(SysError::EINVAL)
                }
            }
            Self::User => {
                if len <= USER_KEY_MAX_PAYLOAD {
                    Ok(())
                } else {
                    Err(SysError::EINVAL)
                }
            }
            Self::Logon => {
                if !description.contains(':') || len > USER_KEY_MAX_PAYLOAD {
                    Err(SysError::EINVAL)
                } else {
                    Ok(())
                }
            }
            Self::BigKey => {
                if len <= BIG_KEY_MAX_PAYLOAD {
                    Ok(())
                } else {
                    Err(SysError::EINVAL)
                }
            }
            Self::ParserOnly => {
                // UNFINISHED: These key types are registered only so add_key()
                // performs Linux-style NULL-payload validation for LTP. Real
                // asymmetric, CIFS/SPNEGO, PKCS#7, and RxRPC parsers are not
                // implemented in this contest subset.
                Ok(())
            }
        }
    }
}

struct KeyEntry {
    kind: KeyKind,
    description: String,
    owner_uid: u32,
    links: Vec<i32>,
    quota_bytes: usize,
    quota_charged: bool,
}

impl KeyEntry {
    fn new(kind: KeyKind, description: String, owner_uid: u32, payload_len: usize) -> Self {
        let quota_charged = kind == KeyKind::User && owner_uid != 0;
        let quota_bytes = if quota_charged {
            user_key_quota_bytes(description.as_str(), payload_len)
        } else {
            0
        };
        Self {
            kind,
            description,
            owner_uid,
            links: Vec::new(),
            quota_bytes,
            quota_charged,
        }
    }
}

#[derive(Default)]
struct UserSpecialKeyrings {
    user: Option<i32>,
    user_session: Option<i32>,
}

#[derive(Default)]
struct UserQuota {
    keys: usize,
    bytes: usize,
}

struct KeyManager {
    next_serial: i32,
    keys: BTreeMap<i32, KeyEntry>,
    user_keyrings: BTreeMap<u32, UserSpecialKeyrings>,
    user_quotas: BTreeMap<u32, UserQuota>,
}

impl KeyManager {
    fn new() -> Self {
        Self {
            next_serial: 1,
            keys: BTreeMap::new(),
            user_keyrings: BTreeMap::new(),
            user_quotas: BTreeMap::new(),
        }
    }

    fn alloc_serial(&mut self) -> SysResult<i32> {
        if self.next_serial == i32::MAX {
            return Err(SysError::EOVERFLOW);
        }
        let serial = self.next_serial;
        self.next_serial += 1;
        Ok(serial)
    }

    fn create_keyring(&mut self, description: String, owner_uid: u32) -> SysResult<i32> {
        let serial = self.alloc_serial()?;
        self.keys.insert(
            serial,
            KeyEntry::new(KeyKind::Keyring, description, owner_uid, 0),
        );
        Ok(serial)
    }

    fn get_user_keyring(
        &mut self,
        uid: u32,
        session: bool,
        create: bool,
    ) -> SysResult<Option<i32>> {
        let existing = self
            .user_keyrings
            .get(&uid)
            .and_then(|ids| if session { ids.user_session } else { ids.user });
        if existing.is_some() || !create {
            return Ok(existing);
        }

        let description = if session {
            format!("_uid_ses.{uid}")
        } else {
            format!("_uid.{uid}")
        };
        let serial = self.create_keyring(description, uid)?;
        let ids = self.user_keyrings.entry(uid).or_default();
        if session {
            ids.user_session = Some(serial);
        } else {
            ids.user = Some(serial);
        }
        Ok(Some(serial))
    }

    fn ensure_keyring(&self, serial: i32) -> SysResult {
        let entry = self.keys.get(&serial).ok_or(SysError::ENOKEY)?;
        if entry.kind == KeyKind::Keyring {
            Ok(0)
        } else {
            Err(SysError::ENOTDIR)
        }
    }

    fn find_in_keyring(
        &self,
        keyring_serial: i32,
        kind: KeyKind,
        description: &str,
    ) -> SysResult<Option<i32>> {
        self.ensure_keyring(keyring_serial)?;
        let links = self
            .keys
            .get(&keyring_serial)
            .map(|entry| entry.links.clone())
            .ok_or(SysError::ENOKEY)?;
        for serial in links {
            if let Some(entry) = self.keys.get(&serial)
                && entry.kind == kind
                && entry.description == description
            {
                return Ok(Some(serial));
            }
        }
        Ok(None)
    }

    fn link_key(&mut self, keyring_serial: i32, key_serial: i32) -> SysResult {
        self.ensure_keyring(keyring_serial)?;
        let keyring = self.keys.get_mut(&keyring_serial).ok_or(SysError::ENOKEY)?;
        if !keyring.links.contains(&key_serial) {
            keyring.links.push(key_serial);
        }
        Ok(0)
    }

    fn reserve_quota(&mut self, entry: &KeyEntry) -> SysResult {
        if !entry.quota_charged {
            return Ok(0);
        }
        let quota = self.user_quotas.entry(entry.owner_uid).or_default();
        let next_keys = quota.keys.checked_add(1).ok_or(SysError::EDQUOT)?;
        let next_bytes = quota
            .bytes
            .checked_add(entry.quota_bytes)
            .ok_or(SysError::EDQUOT)?;
        if next_keys > key_maxkeys() || next_bytes > key_maxbytes() {
            return Err(SysError::EDQUOT);
        }
        quota.keys = next_keys;
        quota.bytes = next_bytes;
        Ok(0)
    }

    fn release_quota(&mut self, entry: &KeyEntry) {
        if !entry.quota_charged {
            return;
        }
        if let Some(quota) = self.user_quotas.get_mut(&entry.owner_uid) {
            quota.keys = quota.keys.saturating_sub(1);
            quota.bytes = quota.bytes.saturating_sub(entry.quota_bytes);
        }
    }

    fn add_key(
        &mut self,
        kind: KeyKind,
        description: String,
        owner_uid: u32,
        payload_len: usize,
        keyring_serial: i32,
    ) -> SysResult<i32> {
        self.ensure_keyring(keyring_serial)?;
        if let Some(serial) = self.find_in_keyring(keyring_serial, kind, description.as_str())? {
            return Ok(serial);
        }

        let serial = self.alloc_serial()?;
        let entry = KeyEntry::new(kind, description, owner_uid, payload_len);
        self.reserve_quota(&entry)?;
        self.keys.insert(serial, entry);
        self.link_key(keyring_serial, serial)?;
        Ok(serial)
    }

    fn search_keyrings(
        &self,
        keyrings: &[i32],
        kind: KeyKind,
        description: &str,
    ) -> SysResult<i32> {
        for keyring in keyrings {
            if let Some(serial) = self.find_in_keyring(*keyring, kind, description)? {
                return Ok(serial);
            }
        }
        Err(SysError::ENOKEY)
    }

    fn release_keyring_tree(&mut self, serial: i32) {
        let Some(entry) = self.keys.remove(&serial) else {
            return;
        };
        for key in self.keys.values_mut() {
            if key.kind == KeyKind::Keyring {
                key.links.retain(|linked| *linked != serial);
            }
        }
        self.release_quota(&entry);
        if entry.kind == KeyKind::Keyring {
            for linked in entry.links {
                self.release_keyring_tree(linked);
            }
        }
    }
}

fn user_key_quota_bytes(description: &str, payload_len: usize) -> usize {
    description
        .len()
        .saturating_add(1)
        .saturating_add(payload_len)
}

fn key_maxkeys() -> usize {
    KEY_MAXKEYS.load(Ordering::Relaxed)
}

fn key_maxbytes() -> usize {
    KEY_MAXBYTES.load(Ordering::Relaxed)
}

fn current_owner_uid() -> u32 {
    current_process().credentials().fsuid
}

fn read_key_kind(type_ptr: *const u8) -> SysResult<KeyKind> {
    let type_name = read_user_c_string(current_user_token(), type_ptr, PATH_MAX)?;
    KeyKind::from_name(type_name.as_str()).ok_or(SysError::ENODEV)
}

fn read_key_description(description: *const u8) -> SysResult<String> {
    read_user_c_string(current_user_token(), description, PATH_MAX)
}

fn validate_payload(payload: *const u8, len: usize) -> SysResult {
    if len == 0 {
        return Ok(0);
    }
    if payload.is_null() {
        return Err(SysError::EFAULT);
    }
    translated_byte_buffer_checked_with_mmap_fault(
        current_user_token(),
        payload,
        len,
        UserBufferAccess::Read,
    )
    .map(|_| 0)
}

fn create_unlinked_keyring(description: String, owner_uid: u32) -> SysResult<i32> {
    KEY_MANAGER.exclusive_session(|manager| manager.create_keyring(description, owner_uid))
}

fn ensure_thread_keyring(create: bool) -> SysResult<i32> {
    let task = current_task().ok_or(SysError::ESRCH)?;
    if let Some(serial) = task.inner_exclusive_access().thread_keyring {
        return Ok(serial);
    }
    if !create {
        return Err(SysError::ENOKEY);
    }
    let serial =
        create_unlinked_keyring(format!("_tid.{}", task.linux_tid()), current_owner_uid())?;
    task.inner_exclusive_access().thread_keyring = Some(serial);
    Ok(serial)
}

fn ensure_process_keyring(create: bool) -> SysResult<i32> {
    let process = current_process();
    if let Some(serial) = process.inner_exclusive_access().process_keyring {
        return Ok(serial);
    }
    if !create {
        return Err(SysError::ENOKEY);
    }
    let serial =
        create_unlinked_keyring(format!("_pid.{}", process.getpid()), current_owner_uid())?;
    process.inner_exclusive_access().process_keyring = Some(serial);
    Ok(serial)
}

fn ensure_session_keyring(create: bool) -> SysResult<i32> {
    let process = current_process();
    if let Some(serial) = process.inner_exclusive_access().session_keyring {
        return Ok(serial);
    }
    if !create {
        return Err(SysError::ENOKEY);
    }
    let serial =
        create_unlinked_keyring(format!("_ses.{}", process.getpid()), current_owner_uid())?;
    process.inner_exclusive_access().session_keyring = Some(serial);
    Ok(serial)
}

fn ensure_user_keyring(session: bool, create: bool) -> SysResult<i32> {
    let uid = current_owner_uid();
    KEY_MANAGER
        .exclusive_session(|manager| manager.get_user_keyring(uid, session, create))?
        .ok_or(SysError::ENOKEY)
}

fn resolve_keyring_id(id: i32, create_special: bool) -> SysResult<i32> {
    match id {
        KEY_SPEC_THREAD_KEYRING => ensure_thread_keyring(create_special),
        KEY_SPEC_PROCESS_KEYRING => ensure_process_keyring(create_special),
        KEY_SPEC_SESSION_KEYRING => ensure_session_keyring(create_special),
        KEY_SPEC_USER_KEYRING => ensure_user_keyring(false, create_special),
        KEY_SPEC_USER_SESSION_KEYRING => ensure_user_keyring(true, create_special),
        serial if serial > 0 => {
            KEY_MANAGER.exclusive_session(|manager| manager.ensure_keyring(serial))?;
            Ok(serial)
        }
        _ => Err(SysError::ENOKEY),
    }
}

fn optional_current_keyrings() -> Vec<i32> {
    let mut keyrings = Vec::new();
    if let Some(task) = current_task()
        && let Some(serial) = task.inner_exclusive_access().thread_keyring
    {
        keyrings.push(serial);
    }
    let process = current_process();
    {
        let inner = process.inner_exclusive_access();
        if let Some(serial) = inner.process_keyring {
            keyrings.push(serial);
        }
        if let Some(serial) = inner.session_keyring {
            keyrings.push(serial);
        }
    }
    let uid = current_owner_uid();
    KEY_MANAGER.exclusive_session(|manager| {
        if let Ok(Some(serial)) = manager.get_user_keyring(uid, false, false) {
            keyrings.push(serial);
        }
        if let Ok(Some(serial)) = manager.get_user_keyring(uid, true, false) {
            keyrings.push(serial);
        }
    });
    keyrings
}

pub fn sys_add_key(
    type_ptr: *const u8,
    description_ptr: *const u8,
    payload: *const u8,
    plen: usize,
    keyring_id: i32,
) -> SysResult {
    let kind = read_key_kind(type_ptr)?;
    let description = read_key_description(description_ptr)?;
    if plen > 0 && payload.is_null() {
        return Err(SysError::EFAULT);
    }
    kind.validate_payload_len(description.as_str(), plen)?;
    validate_payload(payload, plen)?;
    if !kind.can_store() {
        return Err(SysError::ENOTSUP);
    }
    let dest_keyring = resolve_keyring_id(keyring_id, true)?;
    let owner_uid = current_owner_uid();
    let serial = KEY_MANAGER.exclusive_session(|manager| {
        manager.add_key(kind, description, owner_uid, plen, dest_keyring)
    })?;
    Ok(serial as isize)
}

pub fn sys_request_key(
    type_ptr: *const u8,
    description_ptr: *const u8,
    _callout_info: *const u8,
    dest_keyring_id: i32,
) -> SysResult {
    let kind = read_key_kind(type_ptr)?;
    let description = read_key_description(description_ptr)?;
    let visible_keyrings = optional_current_keyrings();
    let serial = KEY_MANAGER.exclusive_session(|manager| {
        manager.search_keyrings(&visible_keyrings, kind, description.as_str())
    })?;
    if dest_keyring_id != 0 {
        let dest = resolve_keyring_id(dest_keyring_id, true)?;
        KEY_MANAGER.exclusive_session(|manager| manager.link_key(dest, serial))?;
    }
    // UNFINISHED: Linux request_key() can invoke /sbin/request-key upcalls and
    // create negative keys. This contest subset only searches existing visible
    // keyrings and returns ENOKEY when no key is already present.
    Ok(serial as isize)
}

pub fn sys_keyctl(
    command: usize,
    arg2: usize,
    arg3: usize,
    _arg4: usize,
    _arg5: usize,
) -> SysResult {
    match command {
        KEYCTL_GET_KEYRING_ID => {
            let create = arg3 != 0;
            Ok(resolve_keyring_id(arg2 as i32, create)? as isize)
        }
        KEYCTL_JOIN_SESSION_KEYRING => {
            let name = if arg2 == 0 {
                format!("_ses.{}", current_process().getpid())
            } else {
                read_user_c_string(current_user_token(), arg2 as *const u8, PATH_MAX)?
            };
            let serial = create_unlinked_keyring(name, current_owner_uid())?;
            current_process().inner_exclusive_access().session_keyring = Some(serial);
            Ok(serial as isize)
        }
        _ => {
            // UNFINISHED: The full Linux keyctl() command surface includes
            // permissions, update/read/link/unlink, revoke, timeout, watch
            // queues, and request-key policy. Only the add_key LTP subset is
            // modeled here.
            Err(SysError::ENOTSUP)
        }
    }
}

pub(crate) fn release_keyring_tree(serial: i32) {
    KEY_MANAGER.exclusive_session(|manager| manager.release_keyring_tree(serial));
}

pub(crate) fn key_users_content() -> String {
    let maxkeys = key_maxkeys();
    let maxbytes = key_maxbytes();
    KEY_MANAGER.exclusive_session(|manager| {
        let mut output = String::new();
        for (uid, quota) in manager.user_quotas.iter() {
            if quota.keys == 0 && quota.bytes == 0 {
                continue;
            }
            output.push_str(&format!(
                "{uid:5}: {usage:5} {keys}/{keys} {keys}/{maxkeys} {bytes}/{maxbytes}\n",
                usage = quota.keys,
                keys = quota.keys,
                bytes = quota.bytes,
            ));
        }
        output
    })
}

pub(crate) fn key_gc_delay_content() -> String {
    format!("{}\n", KEY_GC_DELAY.load(Ordering::Relaxed))
}

pub(crate) fn key_maxkeys_content() -> String {
    format!("{}\n", key_maxkeys())
}

pub(crate) fn key_maxbytes_content() -> String {
    format!("{}\n", key_maxbytes())
}

fn write_usize_sysctl(cell: &AtomicUsize, buf: &[u8], offset: u64) -> usize {
    if offset != 0 {
        return 0;
    }
    let Ok(text) = core::str::from_utf8(buf) else {
        return 0;
    };
    let Ok(value) = text.trim().parse::<usize>() else {
        return 0;
    };
    cell.store(value, Ordering::Relaxed);
    buf.len()
}

pub(crate) fn write_key_gc_delay(buf: &[u8], offset: u64) -> usize {
    write_usize_sysctl(&KEY_GC_DELAY, buf, offset)
}

pub(crate) fn write_key_maxkeys(buf: &[u8], offset: u64) -> usize {
    write_usize_sysctl(&KEY_MAXKEYS, buf, offset)
}

pub(crate) fn write_key_maxbytes(buf: &[u8], offset: u64) -> usize {
    write_usize_sysctl(&KEY_MAXBYTES, buf, offset)
}
