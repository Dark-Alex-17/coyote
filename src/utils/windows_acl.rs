//! The Win32 side of the mesh identity key's owner-only storage: creating a file with a
//! protected single-ACE DACL in the call that creates it, and reading that DACL back. All
//! of the crate's file-security FFI is here so `mesh` keeps its `#![deny(unsafe_code)]`;
//! the identity code sees only safe functions and plain structs.

use std::ffi::c_void;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, GENERIC_WRITE, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
    MAX_PATH,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR_CONTROL, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
#[cfg(test)]
use windows_sys::Win32::Security::{OBJECT_SECURITY_INFORMATION, SetFileSecurityW};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
    GetVolumeInformationByHandleW, READ_CONTROL,
};
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, FILE_PERSISTENT_ACLS};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// What the volume under a file says about its ability to keep a DACL at all.
pub(crate) struct VolumeAcls {
    pub fs_name: String,
    /// False on FAT32 and exFAT, where every file reads back as open to Everyone.
    pub persistent_acls: bool,
}

#[derive(Debug)]
pub(crate) struct AceSummary {
    pub allows: bool,
    pub current_user: bool,
}

#[derive(Debug)]
pub(crate) struct DaclSummary {
    pub owner_is_current_user: bool,
    /// A file without a DACL is open to everyone, which is not the same as an empty one.
    pub dacl_present: bool,
    /// `SE_DACL_PROTECTED`: the directory's inheritable ACEs are kept out.
    pub protected: bool,
    pub aces: Vec<AceSummary>,
}

/// The first way a file's security falls short of owner-only, in the order the checks run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerOnlyProblem {
    OwnerIsNotCurrentUser,
    DaclMissing,
    DaclInherits,
    ForeignAce,
    NoAceForCurrentUser,
}

impl fmt::Display for OwnerOnlyProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OwnerIsNotCurrentUser => "is owned by another user",
            Self::DaclMissing => "has no DACL, so every user can read it",
            Self::DaclInherits => "inherits permissions from its directory",
            Self::ForeignAce => {
                "carries an access-control entry that is not a plain allow for the current user"
            }
            Self::NoAceForCurrentUser => "grants access to nobody",
        })
    }
}

/// Memory a Win32 call handed out with `LocalAlloc`, freed exactly once however the
/// function holding it returns.
struct LocalAllocation(HLOCAL);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a Win32 call that documents `LocalFree` as its
        // release, and this guard is the only place it is freed.
        unsafe {
            LocalFree(self.0);
        }
    }
}

/// The calling process's user SID. `GetTokenInformation` writes a `TOKEN_USER` whose SID
/// pointer aims back into the same buffer, so the buffer is what is kept and the SID is
/// read out of it on demand.
struct CurrentUser {
    /// `usize` rather than `u8` so the pointer inside `TOKEN_USER` is aligned.
    token_user: Vec<usize>,
}

impl CurrentUser {
    fn query() -> io::Result<Self> {
        let mut raw: HANDLE = null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is never closed; the
        // token handle it opens is owned by the guard below.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) };
        if opened == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `OpenProcessToken` just returned this handle open, and nothing else
        // closes it.
        let token = unsafe { OwnedHandle::from_raw_handle(raw) };

        let mut needed = 0u32;
        // SAFETY: a null buffer of length zero is the documented way to size the result;
        // the call writes only `needed`.
        let sized = unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut needed)
        };
        if sized != 0 {
            return Err(io::Error::other(
                "GetTokenInformation reported success for a zero-length TOKEN_USER buffer",
            ));
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(err);
        }

        let mut token_user = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        let capacity = (token_user.len() * size_of::<usize>()) as u32;
        let mut written = 0u32;
        // SAFETY: the buffer holds at least `needed` bytes, is passed with its true
        // capacity, and outlives the call.
        let filled = unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                token_user.as_mut_ptr().cast::<c_void>(),
                capacity,
                &mut written,
            )
        };
        if filled == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { token_user })
    }

    fn sid(&self) -> PSID {
        // SAFETY: `query` filled the buffer with a `TOKEN_USER` whose SID lives further
        // along in the same buffer, and the buffer lives as long as `self`.
        unsafe { (*self.token_user.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    /// The `S-1-5-...` form, which SDDL and `icacls` both take.
    fn sid_string(&self) -> io::Result<String> {
        let mut text: *mut u16 = null_mut();
        // SAFETY: `sid` is valid for the life of `self`; the string the call allocates is
        // released by the guard below.
        let converted = unsafe { ConvertSidToStringSidW(self.sid(), &mut text) };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        let _allocation = LocalAllocation(text.cast::<c_void>());
        // SAFETY: the call NUL-terminates the string, which stays allocated until
        // `_allocation` drops at the end of this function.
        Ok(unsafe { read_wide(text) })
    }
}

/// # Safety
///
/// `text` must point to a NUL-terminated UTF-16 string that stays allocated for the call.
unsafe fn read_wide(text: *const u16) -> String {
    let mut len = 0;
    // SAFETY: the caller guarantees a NUL inside the allocation, so every read before it
    // is in bounds.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` code units were just read from this pointer.
    let units = unsafe { std::slice::from_raw_parts(text, len) };
    String::from_utf16_lossy(units)
}

/// A self-relative security descriptor built from SDDL, freed when dropped.
struct Descriptor(LocalAllocation);

impl Descriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(once(0)).collect();
        let mut psd: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: `wide` is NUL-terminated and outlives the call; the descriptor it
        // allocates is released by the guard. The size out-parameter is optional.
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut psd,
                null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(LocalAllocation(psd)))
    }

    fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0.0
    }
}

/// Owner set to `sid`, and a DACL that is protected (`P`: no inheritance from the
/// directory) and holds one allow ACE granting `sid` everything. The explicit `O:` is
/// load-bearing: an elevated token's default owner is typically `BUILTIN\Administrators`,
/// which `owner_only_problem` would refuse as `OwnerIsNotCurrentUser` on the next load.
fn owner_only_sddl(sid: &str) -> String {
    format!("O:{sid}D:P(A;;GA;;;{sid})")
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(once(0)).collect()
}

pub(crate) fn current_user_sid_string() -> io::Result<String> {
    CurrentUser::query()?.sid_string()
}

/// Creates `path` with an owner-only descriptor in the one `CreateFileW` call, so there is
/// no instant at which the file exists with wider permissions. `CREATE_NEW` refuses an
/// existing file with `ErrorKind::AlreadyExists`; with `FILE_FLAG_OPEN_REPARSE_POINT` that
/// refusal also covers a symlink at `path`, which would otherwise be followed and its
/// target created. The handle is opened with no sharing, so it must be dropped before the
/// file can be removed.
pub(crate) fn create_owner_only(path: &Path) -> io::Result<File> {
    let user = CurrentUser::query()?;
    let descriptor = Descriptor::from_sddl(&owner_only_sddl(&user.sid_string()?))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and `attributes` points at a descriptor that both
    // outlive the call; the kernel copies the descriptor onto the new file.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the handle was just created, nothing else owns it, and `File` closes it.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Opens `path` with `READ_CONTROL` alone, enough for `inspect` and nothing else. The
/// owner is implicitly granted that right unless an `OWNER RIGHTS` ACE overrides it, so a
/// key whose DACL denies its owner the read `File::open` asks for can still have that DACL
/// reported; a key owned by someone else may not open even for this.
pub(crate) fn open_for_security_read(path: &Path) -> io::Result<File> {
    OpenOptions::new().access_mode(READ_CONTROL).open(path)
}

pub(crate) fn volume_acls(file: &File) -> io::Result<VolumeAcls> {
    let mut flags = 0u32;
    let mut name = [0u16; MAX_PATH as usize + 1];
    // SAFETY: the handle is open for as long as `file` is borrowed; the name buffer is
    // passed with its own length and the other out-parameters are documented as optional.
    let queried = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if queried == 0 {
        return Err(io::Error::last_os_error());
    }
    let len = name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len());
    Ok(VolumeAcls {
        fs_name: String::from_utf16_lossy(&name[..len]),
        persistent_acls: flags & FILE_PERSISTENT_ACLS != 0,
    })
}

/// Reads back the owner and DACL of the file `file` is open on, so the verdict is about
/// the same file whose bytes the handle reads and not whatever the name resolves to by
/// then. The handle must carry `READ_CONTROL`, which every `File::open`,
/// `create_owner_only` and `open_for_security_read` handle does. Only ACE types this
/// module writes are looked into; any other type is reported as not allowing the current
/// user without its layout being interpreted.
pub(crate) fn inspect(file: &File) -> io::Result<DaclSummary> {
    let user = CurrentUser::query()?;
    let mut owner: PSID = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut psd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: the handle is open for as long as `file` is borrowed; the group and SACL
    // out-parameters are optional and the requested information does not include them.
    // On success `psd` is one allocation that `owner` and `dacl` point into, released by
    // the guard below.
    let code = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut psd,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    let _descriptor = LocalAllocation(psd);

    let mut control: SECURITY_DESCRIPTOR_CONTROL = 0;
    let mut revision = 0u32;
    // SAFETY: `psd` is a valid descriptor until `_descriptor` drops.
    let read = unsafe { GetSecurityDescriptorControl(psd, &mut control, &mut revision) };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    let protected = control & SE_DACL_PROTECTED != 0;
    // SAFETY: `owner` is null or points into the descriptor, and `user.sid()` lives as
    // long as `user`; `EqualSid` only reads both.
    let owner_is_current_user = !owner.is_null() && unsafe { EqualSid(owner, user.sid()) != 0 };
    if dacl.is_null() {
        return Ok(DaclSummary {
            owner_is_current_user,
            dacl_present: false,
            protected,
            aces: Vec::new(),
        });
    }

    let mut info = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `dacl` points into the descriptor; `info` is passed with its own size and
    // the matching information class.
    let counted = unsafe {
        GetAclInformation(
            dacl,
            (&raw mut info).cast::<c_void>(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if counted == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut aces = Vec::with_capacity(info.AceCount as usize);
    for index in 0..info.AceCount {
        let mut ace: *mut c_void = null_mut();
        // SAFETY: `index` is below the count the ACL just reported; the returned pointer
        // aims into the ACL, which lives as long as the descriptor.
        let found = unsafe { GetAce(dacl, index, &mut ace) };
        if found == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every ACE starts with an `ACE_HEADER`.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        let allows = header.AceType == ACCESS_ALLOWED_ACE_TYPE as u8;
        // SAFETY: only entered when the header says this is an `ACCESS_ALLOWED_ACE`, whose
        // SID begins at `SidStart` and runs to the end of the ACE; `EqualSid` only reads it.
        let current_user = allows
            && unsafe {
                let sid = (&raw mut (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast::<c_void>();
                EqualSid(sid, user.sid()) != 0
            };
        aces.push(AceSummary {
            allows,
            current_user,
        });
    }
    Ok(DaclSummary {
        owner_is_current_user,
        dacl_present: true,
        protected,
        aces,
    })
}

/// Whether the file `file` is open on is readable by the current user alone. The outer
/// error is a failed read of the security information; the inner one is the first
/// shortfall found. The handle needs `READ_CONTROL`, as for `inspect`.
pub(crate) fn check_owner_only(file: &File) -> io::Result<Result<(), OwnerOnlyProblem>> {
    let summary = inspect(file)?;
    Ok(owner_only_problem(&summary).map_or(Ok(()), Err))
}

fn owner_only_problem(summary: &DaclSummary) -> Option<OwnerOnlyProblem> {
    if !summary.owner_is_current_user {
        return Some(OwnerOnlyProblem::OwnerIsNotCurrentUser);
    }
    if !summary.dacl_present {
        return Some(OwnerOnlyProblem::DaclMissing);
    }
    if !summary.protected {
        return Some(OwnerOnlyProblem::DaclInherits);
    }
    if summary
        .aces
        .iter()
        .any(|ace| !(ace.allows && ace.current_user))
    {
        return Some(OwnerOnlyProblem::ForeignAce);
    }
    if summary.aces.is_empty() {
        return Some(OwnerOnlyProblem::NoAceForCurrentUser);
    }
    None
}

/// Replaces the DACL of `path` with the one `sddl` describes, leaving the owner alone.
/// Written through `SetFileSecurityW` so a test that widens a key does not go through the
/// same calls the check under test reads with.
#[cfg(test)]
pub(crate) fn widen_with_sddl(path: &Path, sddl: &str) -> io::Result<()> {
    set_security_with_sddl(path, sddl, DACL_SECURITY_INFORMATION)
}

/// Replaces the owner of `path` with the one `sddl` names, leaving the DACL alone. Only a
/// SID the token may assign as owner is accepted: the current user, or a group it holds
/// with `SE_GROUP_OWNER`, which for an elevated Administrators member includes
/// `BUILTIN\Administrators`; anything else fails with `ERROR_INVALID_OWNER`.
#[cfg(test)]
pub(crate) fn set_owner_with_sddl(path: &Path, sddl: &str) -> io::Result<()> {
    set_security_with_sddl(path, sddl, OWNER_SECURITY_INFORMATION)
}

#[cfg(test)]
fn set_security_with_sddl(
    path: &Path,
    sddl: &str,
    security_information: OBJECT_SECURITY_INFORMATION,
) -> io::Result<()> {
    let descriptor = Descriptor::from_sddl(sddl)?;
    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and the descriptor outlives the call.
    let set = unsafe { SetFileSecurityW(wide.as_ptr(), security_information, descriptor.as_ptr()) };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_only() -> DaclSummary {
        DaclSummary {
            owner_is_current_user: true,
            dacl_present: true,
            protected: true,
            aces: vec![AceSummary {
                allows: true,
                current_user: true,
            }],
        }
    }

    #[test]
    fn owner_only_problem_accepts_the_summary_create_owner_only_writes() {
        assert_eq!(owner_only_problem(&owner_only()), None);
    }

    #[test]
    fn owner_only_problem_reports_the_owner_before_anything_else() {
        let summary = DaclSummary {
            owner_is_current_user: false,
            dacl_present: false,
            protected: false,
            aces: vec![AceSummary {
                allows: true,
                current_user: false,
            }],
        };
        assert_eq!(
            owner_only_problem(&summary),
            Some(OwnerOnlyProblem::OwnerIsNotCurrentUser)
        );
    }

    #[test]
    fn owner_only_problem_reports_a_missing_dacl() {
        let summary = DaclSummary {
            dacl_present: false,
            aces: Vec::new(),
            ..owner_only()
        };
        assert_eq!(
            owner_only_problem(&summary),
            Some(OwnerOnlyProblem::DaclMissing)
        );
    }

    #[test]
    fn owner_only_problem_reports_inheritance_before_the_aces_it_brought_in() {
        let summary = DaclSummary {
            protected: false,
            aces: vec![AceSummary {
                allows: true,
                current_user: false,
            }],
            ..owner_only()
        };
        assert_eq!(
            owner_only_problem(&summary),
            Some(OwnerOnlyProblem::DaclInherits)
        );
    }

    #[test]
    fn owner_only_problem_reports_an_allow_for_another_principal() {
        let summary = DaclSummary {
            aces: vec![
                AceSummary {
                    allows: true,
                    current_user: true,
                },
                AceSummary {
                    allows: true,
                    current_user: false,
                },
            ],
            ..owner_only()
        };
        assert_eq!(
            owner_only_problem(&summary),
            Some(OwnerOnlyProblem::ForeignAce)
        );
    }

    #[test]
    fn owner_only_problem_treats_a_deny_ace_as_foreign() {
        let summary = DaclSummary {
            aces: vec![AceSummary {
                allows: false,
                current_user: false,
            }],
            ..owner_only()
        };
        assert_eq!(
            owner_only_problem(&summary),
            Some(OwnerOnlyProblem::ForeignAce)
        );
    }

    #[test]
    fn owner_only_problem_reports_an_empty_dacl() {
        let summary = DaclSummary {
            aces: Vec::new(),
            ..owner_only()
        };
        assert_eq!(
            owner_only_problem(&summary),
            Some(OwnerOnlyProblem::NoAceForCurrentUser)
        );
    }

    #[test]
    fn volume_acls_reports_the_temp_volume_as_keeping_acls() {
        let path = std::env::temp_dir().join(format!("coyote-volume-acls-{}", std::process::id()));
        let file = create_owner_only(&path).unwrap();

        let volume = volume_acls(&file).unwrap();

        assert!(volume.persistent_acls, "{}", volume.fs_name);
        assert!(
            matches!(volume.fs_name.as_str(), "NTFS" | "ReFS"),
            "{}",
            volume.fs_name
        );
        drop(file);
        std::fs::remove_file(&path).unwrap();
    }
}
