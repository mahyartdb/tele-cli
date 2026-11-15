use std::path::Path;

pub fn create_dir_private(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "refusing to follow symlink at {}",
                path.display()
            )));
        }
        #[cfg(unix)]
        restrict(path, 0o700)?;
        #[cfg(windows)]
        set_user_only_dacl(path, true)?;
        return Ok(());
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_name().ok_or_else(|| {
        std::io::Error::other(format!(
            "cannot create private dir: no file name in {}",
            path.display()
        ))
    })?;
    let rand: u16 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u16)
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}_tmp-{}-{}",
        stem.to_string_lossy(),
        std::process::id(),
        rand
    ));
    if std::fs::symlink_metadata(&tmp).is_ok() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    std::fs::create_dir_all(&tmp)?;
    if std::fs::symlink_metadata(&tmp)?.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "refusing to follow symlink at {}",
            tmp.display()
        )));
    }
    #[cfg(unix)]
    restrict(&tmp, 0o700)?;
    #[cfg(windows)]
    set_user_only_dacl(&tmp, true)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => {
            if std::fs::symlink_metadata(path)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                return Err(std::io::Error::other(format!(
                    "refusing to follow symlink at {}",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                if std::fs::symlink_metadata(path)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    return Err(std::io::Error::other(format!(
                        "refusing to follow symlink at {}",
                        path.display()
                    )));
                }
                #[cfg(unix)]
                restrict(path, 0o700)?;
                #[cfg(windows)]
                set_user_only_dacl(path, true)?;
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(unix)]
pub fn write_file_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true).mode(0o600);
    let mut file = opts.open(path)?;
    use std::io::Write as _;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(windows)]
pub fn write_file_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    {
        Ok(f) => {
            set_user_only_dacl(path, false)?;
            f
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
                return Err(std::io::Error::other(format!(
                    "refusing to follow symlink at {}",
                    path.display()
                )));
            }
            let f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)?;
            set_user_only_dacl(path, false)?;
            f
        }
        Err(e) => return Err(e),
    };
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub fn write_file_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(unix)]
pub fn create_file_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true).mode(0o600);
    opts.open(path)
}

#[cfg(windows)]
pub fn create_file_private(path: &Path) -> std::io::Result<std::fs::File> {
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
    {
        Ok(f) => {
            set_user_only_dacl(path, false)?;
            Ok(f)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
                return Err(std::io::Error::other(format!(
                    "refusing to follow symlink at {}",
                    path.display()
                )));
            }
            let f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(path)?;
            set_user_only_dacl(path, false)?;
            Ok(f)
        }
        Err(e) => Err(e),
    }
}

#[cfg(not(any(unix, windows)))]
pub fn create_file_private(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

#[cfg(unix)]
pub fn restrict_file_private(path: &Path) -> std::io::Result<()> {
    restrict(path, 0o600)
}

#[cfg(windows)]
pub fn restrict_file_private(path: &Path) -> std::io::Result<()> {
    set_user_only_dacl(path, false)
}

#[cfg(test)]
static PERMISSION_WRITES: std::sync::Mutex<Vec<std::path::PathBuf>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn record_permission_write(path: &Path) {
    PERMISSION_WRITES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(path.to_path_buf());
}

#[cfg(test)]
pub(crate) fn permission_write_count(path: &Path) -> usize {
    PERMISSION_WRITES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|written| written.as_path() == path)
        .count()
}

#[cfg(windows)]
fn current_user_sid() -> std::io::Result<&'static [u8]> {
    static SID: std::sync::OnceLock<Result<Vec<u8>, String>> = std::sync::OnceLock::new();
    match SID.get_or_init(read_current_user_sid) {
        Ok(bytes) => Ok(bytes.as_slice()),
        Err(message) => Err(std::io::Error::other(message.clone())),
    }
}

#[cfg(windows)]
fn read_current_user_sid() -> Result<Vec<u8>, String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetLengthSid, GetTokenInformation, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: `token` comes from OpenProcessToken and is closed on every
    // return path; `buffer` is sized from the API's own size query and its
    // pointer plus length are passed together so GetTokenInformation cannot
    // overrun it; `read_unaligned` reads a TOKEN_USER the API just wrote
    // without assuming buffer alignment; the SID bytes are copied out of the
    // buffer before either the buffer or the token is released, so the cached
    // copy is self-contained.
    unsafe {
        let mut token = HANDLE::default();
        if let Err(e) = OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) {
            return Err(e.to_string());
        }

        let mut size = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
        let mut buffer = vec![0u8; size as usize];
        let filled = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut _),
            size,
            &mut size,
        );

        let result = match filled {
            Err(e) => Err(e.to_string()),
            Ok(()) => {
                let token_user = std::ptr::read_unaligned(buffer.as_ptr() as *const TOKEN_USER);
                let sid = PSID(token_user.User.Sid.0);
                let len = GetLengthSid(sid) as usize;
                Ok(std::slice::from_raw_parts(sid.0 as *const u8, len).to_vec())
            }
        };

        let _ = CloseHandle(token);
        result
    }
}

#[cfg(windows)]
fn dacl_is_owner_only(path: &Path, inheritable: bool) -> std::io::Result<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL, WIN32_ERROR};
    use windows::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows::Win32::Security::{
        GetAce, GetLengthSid, GetSecurityDescriptorControl, IsValidSid, ACCESS_ALLOWED_ACE,
        ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    };
    use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    // ACCESS_ALLOWED_ACE_TYPE lives in Win32::System::SystemServices, which is
    // not among this crate's enabled windows features.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    let owner_sid = current_user_sid()?;
    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();

    // SAFETY: `path_wide` is NUL-terminated and alive for the call; `dacl` and
    // `descriptor` are out-parameters that the API fills in; the returned
    // descriptor is freed exactly once below, after the pointers into it are no
    // longer used.
    let status = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
    };
    if status != WIN32_ERROR(0) {
        return Err(std::io::Error::other(format!(
            "GetNamedSecurityInfoW failed: {}",
            status.0
        )));
    }

    let mut control = 0u16;
    let mut revision = 0u32;
    let control_known =
        unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) }.is_ok();
    let mut owner_only = control_known && (control & SE_DACL_PROTECTED.0) != 0 && !dacl.is_null();
    if owner_only {
        // SAFETY: `dacl` is non-null and points into `descriptor`.
        owner_only = unsafe { (*dacl).AceCount } == 1;
    }

    if owner_only {
        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: the ACE index is within the ACL's AceCount, and the pointer
        // GetAce returns stays valid until `descriptor` is freed below.
        if unsafe { GetAce(dacl, 0, &mut ace) }.is_ok() {
            let header = unsafe { std::ptr::read_unaligned(ace as *const ACE_HEADER) };
            let allowed = unsafe { std::ptr::read_unaligned(ace as *const ACCESS_ALLOWED_ACE) };
            let expected_flags = if inheritable {
                (CONTAINER_INHERIT_ACE.0 | OBJECT_INHERIT_ACE.0) as u8
            } else {
                0
            };
            let ace_sid = PSID(unsafe {
                (ace as *const u8).add(std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart))
            } as *mut _);
            // SAFETY: `ace_sid` points at the ACE's own SID; IsValidSid checks
            // its structure before GetLengthSid and the byte comparison read
            // exactly the SID length the ACE declares.
            let sid_matches = if unsafe { IsValidSid(ace_sid) }.as_bool() {
                let len = unsafe { GetLengthSid(ace_sid) } as usize;
                len == owner_sid.len()
                    && unsafe { std::slice::from_raw_parts(ace_sid.0 as *const u8, len) }
                        == owner_sid
            } else {
                false
            };
            owner_only = header.AceType == ACCESS_ALLOWED_ACE_TYPE
                && header.AceFlags == expected_flags
                && allowed.Mask == FILE_ALL_ACCESS.0
                && sid_matches;
        } else {
            owner_only = false;
        }
    }

    // SAFETY: `descriptor` came from GetNamedSecurityInfoW, which allocates with
    // LocalAlloc, so LocalFree releases it; no pointer into it is used after.
    unsafe {
        let _ = LocalFree(HLOCAL(descriptor.0));
    }

    Ok(owner_only)
}

#[cfg(windows)]
fn set_user_only_dacl(path: &Path, inheritable: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE,
        SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
    };
    use windows::Win32::Security::{
        ACE_FLAGS, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    };
    use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    // A check that cannot read the current ACL (missing path, denied read)
    // falls through to the rewrite, so the error a caller sees still comes from
    // the write that has always produced it.
    if dacl_is_owner_only(path, inheritable).unwrap_or(false) {
        return Ok(());
    }

    let user_sid = current_user_sid()?;
    let sid = PSID(user_sid.as_ptr() as *mut _);

    // SAFETY: `user_sid` is the process user's SID, cached for the life of the
    // process, so it outlives its use in SetEntriesInAclW; `new_acl` allocated
    // by SetEntriesInAclW is freed exactly once via LocalFree when non-null;
    // `path_wide` is NUL-terminated and alive for the SetNamedSecurityInfoW
    // call.
    unsafe {
        let trustee = TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: PWSTR(sid.0 as *mut u16),
        };

        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS.0,
            grfAccessMode: SET_ACCESS,
            grfInheritance: if inheritable {
                CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
            } else {
                ACE_FLAGS(0)
            },
            Trustee: trustee,
        };

        let mut new_acl: *mut ACL = std::ptr::null_mut();
        let result = SetEntriesInAclW(Some(&[ea]), None, &mut new_acl);
        if result.0 != 0 {
            return Err(std::io::Error::other(format!(
                "SetEntriesInAclW failed: {}",
                result.0
            )));
        }

        let path_wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        #[cfg(test)]
        record_permission_write(path);

        let result = SetNamedSecurityInfoW(
            PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(new_acl),
            None,
        );

        if !new_acl.is_null() {
            let _ = LocalFree(HLOCAL(new_acl as *mut _));
        }

        if result.0 != 0 {
            return Err(std::io::Error::other(format!(
                "SetNamedSecurityInfoW failed: {}",
                result.0
            )));
        }

        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
pub fn restrict_file_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let current = std::fs::metadata(path)?.permissions().mode();
    if current & 0o7777 == mode & 0o7777 {
        return Ok(());
    }
    #[cfg(test)]
    record_permission_write(path);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

pub fn path_under_guard(candidate: &Path, guard: &Path) -> bool {
    #[cfg(windows)]
    {
        let candidate: Vec<_> = candidate.components().collect();
        let guard: Vec<_> = guard.components().collect();
        candidate.len() >= guard.len()
            && candidate.iter().zip(&guard).all(|(c, g)| {
                c.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&g.as_os_str().to_string_lossy())
            })
    }
    #[cfg(not(windows))]
    {
        candidate.starts_with(guard)
    }
}

pub fn resolve_for_guard(path: &Path) -> std::path::PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    let resolved = loop {
        if let Ok(canonical) = std::fs::canonicalize(cursor) {
            let mut rebuilt = canonical;
            for part in tail.iter().rev() {
                rebuilt.push(part);
            }
            break rebuilt;
        }
        let last = match cursor.components().next_back() {
            None => break path.to_path_buf(),
            Some(c) => c,
        };
        match last {
            std::path::Component::Normal(name) => tail.push(name.to_os_string()),
            std::path::Component::ParentDir => tail.push("..".into()),
            std::path::Component::CurDir => tail.push(".".into()),
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                break path.to_path_buf()
            }
        }
        match cursor.parent() {
            None => break path.to_path_buf(),
            Some(parent) => cursor = parent,
        }
    };
    let normalized = lexical_normalize(&resolved);
    #[cfg(windows)]
    let normalized = strip_verbatim_prefix(normalized);
    normalized
}

fn lexical_normalize(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut prefix: Option<std::ffi::OsString> = None;
    let mut has_root = false;
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => prefix = Some(p.as_os_str().to_os_string()),
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() && !has_root && prefix.is_none() {
                    stack.push("..".into());
                }
            }
            Component::Normal(c) => stack.push(c.to_os_string()),
        }
    }
    let mut out = std::path::PathBuf::new();
    if let Some(p) = prefix {
        out.push(p);
    }
    if has_root {
        out.push(Component::RootDir.as_os_str());
    }
    for part in stack {
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: std::path::PathBuf) -> std::path::PathBuf {
    let os = path.into_os_string();
    let bytes = os.as_encoded_bytes();
    let prefix = br"\\?\";
    if bytes.starts_with(prefix) {
        let stripped = &bytes[prefix.len()..];
        // SAFETY: the split is at `prefix.len()` (4), an ASCII boundary of
        // the original OS bytes, so the remainder is still valid platform
        // encoding and reconstructs the same path minus the verbatim prefix.
        std::path::PathBuf::from(unsafe {
            std::ffi::OsString::from_encoded_bytes_unchecked(stripped.to_vec())
        })
    } else {
        std::path::PathBuf::from(os)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("telecli-fs-{tag}-{}", std::process::id()))
    }

    #[cfg(unix)]
    #[test]
    fn create_dir_private_sets_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_path("dir");
        let _ = std::fs::remove_dir_all(&dir);
        create_dir_private(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restrict_file_private_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_path("file");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.session");
        std::fs::write(&file, b"x").unwrap();
        restrict_file_private(&file).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn restrict_file_private_skips_rewrite_when_already_private() {
        let dir = temp_path("no-rewrite");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.session");
        std::fs::write(&file, b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        #[cfg(windows)]
        assert!(
            !dacl_is_owner_only(&file, false).unwrap(),
            "a file created in a shared directory inherits a DACL that is not owner-only"
        );
        let before = permission_write_count(&file);
        restrict_file_private(&file).unwrap();
        let tightened = permission_write_count(&file);
        assert_eq!(
            tightened,
            before + 1,
            "an exposed session file must be tightened"
        );
        restrict_file_private(&file).unwrap();
        assert_eq!(
            permission_write_count(&file),
            tightened,
            "a file that is already owner-only must not be rewritten"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        #[cfg(windows)]
        assert!(dacl_is_owner_only(&file, false).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn restrict_file_private_rewrites_when_mode_widened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_path("rewiden");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.session");
        std::fs::write(&file, b"x").unwrap();
        restrict_file_private(&file).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = permission_write_count(&file);
        restrict_file_private(&file).unwrap();
        assert_eq!(permission_write_count(&file), before + 1);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    fn grant_everyone_full_access(path: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PWSTR;
        use windows::Win32::Foundation::{LocalFree, HLOCAL, WIN32_ERROR};
        use windows::Win32::Security::Authorization::{
            SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE,
            SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
        };
        use windows::Win32::Security::{
            ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        };
        use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

        // S-1-1-0 (Everyone) as a self-relative SID.
        let everyone: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS.0,
            grfAccessMode: SET_ACCESS,
            grfInheritance: windows::Win32::Security::ACE_FLAGS(0),
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: PWSTR(everyone.as_ptr() as *mut u16),
            },
        };
        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: the trustee SID lives in `everyone` for the whole call and
        // SetEntriesInAclW copies it into the ACL it allocates; that ACL is
        // freed exactly once below and is passed to SetNamedSecurityInfoW while
        // still alive.
        unsafe {
            assert_eq!(SetEntriesInAclW(Some(&[ea]), None, &mut acl).0, 0);
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            wide.push(0);
            let status = SetNamedSecurityInfoW(
                windows::core::PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                windows::Win32::Security::PSID(std::ptr::null_mut()),
                windows::Win32::Security::PSID(std::ptr::null_mut()),
                Some(acl),
                None,
            );
            let _ = LocalFree(HLOCAL(acl as *mut _));
            assert_eq!(status, WIN32_ERROR(0));
        }
    }

    #[cfg(windows)]
    #[test]
    fn restrict_file_private_rewrites_when_acl_changes() {
        let dir = temp_path("acl-change");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.session");
        std::fs::write(&file, b"x").unwrap();
        restrict_file_private(&file).unwrap();
        assert!(dacl_is_owner_only(&file, false).unwrap());
        grant_everyone_full_access(&file);
        assert!(
            !dacl_is_owner_only(&file, false).unwrap(),
            "an ACE granted to Everyone is not the owner-only ACL"
        );
        let before = permission_write_count(&file);
        restrict_file_private(&file).unwrap();
        assert_eq!(permission_write_count(&file), before + 1);
        assert!(dacl_is_owner_only(&file, false).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn current_user_sid_is_cached_and_well_formed() {
        let first = current_user_sid().unwrap();
        let second = current_user_sid().unwrap();
        assert_eq!(
            first.as_ptr(),
            second.as_ptr(),
            "the SID must be read from the process token once per process"
        );
        assert_eq!(first[0], 1, "SID revision must be 1");
        let sub_authorities = first[1] as usize;
        assert_eq!(first.len(), 8 + 4 * sub_authorities);
        assert!(
            first[2..8].iter().any(|b| *b != 0),
            "the identifier authority must be non-zero"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_dir_private_tightens_existing_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_path("tighten");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_dir_private(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_under_guard_requires_component_prefix() {
        let guard = std::path::Path::new("guard").join("sub");
        let under = guard.join("file.txt");
        assert!(path_under_guard(&under, &guard));
        assert!(!path_under_guard(&guard, &under));
        let sibling = std::path::Path::new("guard").join("sub2").join("file.txt");
        assert!(!path_under_guard(&sibling, &guard));
        let prefix_like = std::path::Path::new("guard").join("submarine").join("x");
        assert!(!path_under_guard(&prefix_like, &guard));
        assert!(!path_under_guard(std::path::Path::new("guard"), &guard));
    }

    #[cfg(windows)]
    #[test]
    fn path_under_guard_ignores_ascii_case_on_windows() {
        let guard = std::path::Path::new(r"C:\Users\Alice\AppData\Roaming\TeleCli");
        let under =
            std::path::Path::new(r"c:\users\alice\appdata\roaming\telecli\sessions\me.session");
        assert!(path_under_guard(under, guard));
        let lookalike = std::path::Path::new(r"C:\USERS\ALICE\APPDATA\ROAMING\TELECLI-BAK\x");
        assert!(!path_under_guard(lookalike, guard));
    }

    #[test]
    fn resolve_for_guard_rebuilds_nonexisting_tail() {
        let base = temp_path("resolve");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let deep = base.join("a").join("b").join("c.txt");
        let resolved = resolve_for_guard(&deep);
        let resolved_lower = resolved.to_string_lossy().to_lowercase();
        // The existing prefix is canonicalized (may use 8.3 short names on
        // Windows), so the expectation is the canonical base, not the raw one.
        let canon_base = std::fs::canonicalize(&base).unwrap();
        let canon_base = canon_base.to_string_lossy().to_string();
        #[cfg(windows)]
        let canon_base = canon_base.trim_start_matches(r"\\?\");
        let base_lower = canon_base.to_lowercase();
        assert!(resolved_lower.starts_with(&base_lower), "{resolved:?}");
        assert!(
            resolved_lower.ends_with(r"a\b\c.txt") || resolved_lower.ends_with("a/b/c.txt"),
            "{resolved:?}"
        );
        let canon = std::fs::canonicalize(&base).unwrap();
        let canon = canon.to_string_lossy();
        #[cfg(windows)]
        let canon = canon.trim_start_matches(r"\\?\");
        assert_eq!(resolve_for_guard(&base).to_string_lossy(), canon);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn resolve_for_guard_normalizes_dot_and_dot_dot() {
        let base = temp_path("resolve-dot");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let with_dot = base.join(".").join("a").join(".").join("b.txt");
        let resolved_dot = resolve_for_guard(&with_dot);
        assert!(
            !resolved_dot.to_string_lossy().contains(".."),
            "dot should be stripped: {resolved_dot:?}"
        );
        assert!(
            resolved_dot.ends_with(std::path::Path::new("a").join("b.txt")),
            "resolved {resolved_dot:?} should end with a/b.txt"
        );
        let with_dotdot = base.join("tmp").join("..").join("a.txt");
        let resolved_dd = resolve_for_guard(&with_dotdot);
        assert!(
            !resolved_dd.to_string_lossy().contains(".."),
            "dot-dot should be normalized: {resolved_dd:?}"
        );
        assert!(
            resolved_dd.ends_with("a.txt"),
            "resolved {resolved_dd:?} should end with a.txt"
        );
        let canon = std::fs::canonicalize(&base).unwrap();
        let expected =
            std::path::Path::new(canon.to_string_lossy().trim_start_matches(r"\\?\")).join("a.txt");
        assert_eq!(
            resolved_dd
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_lowercase(),
            expected
                .to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_lowercase()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_path_traversal_blocked() {
        let _guard = crate::config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base = temp_path("traversal-guard");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::env::set_var("TELE_APP_DIR", &base);
        let app_dir = resolve_for_guard(&crate::config::app_data_dir());
        let parent = base.parent().unwrap().to_path_buf();
        let traversal = parent
            .join("dummy")
            .join("..")
            .join(base.file_name().unwrap())
            .join(".env");
        let candidate = traversal.to_string_lossy().to_string();
        let resolved = resolve_for_guard(std::path::Path::new(&candidate));
        assert!(
            path_under_guard(&resolved, &app_dir),
            "resolved {resolved:?} should be under guard {app_dir:?} for candidate {candidate:?}"
        );
        let err = crate::commands::msg::validate::validate_upload_path_inner(&candidate, true)
            .unwrap_err();
        assert!(
            err.to_string().contains("app data directory"),
            "traversal should be blocked: {err}"
        );
        std::env::remove_var("TELE_APP_DIR");
        let _ = std::fs::remove_dir_all(&base);
    }
}
