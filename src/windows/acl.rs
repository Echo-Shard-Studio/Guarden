use std::path::Path;
use std::ptr::null_mut;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::*;
use windows::Win32::Security::*;
use windows::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, SECURITY_DESCRIPTOR_REVISION};
use windows::core::{HSTRING, PWSTR, Result};

/// Grant `mask` on `path` to `sid`. With `inherit`, a directory's ACE inherits to everything below;
/// without, only `path` itself changes and nothing below is walked.
/// Does nothing if the access is already there, so repeat launches don't re-walk big trees, or if
/// every container already has it (ALL APPLICATION PACKAGES: Program Files, where we can't write).
pub fn grant(path: &Path, sid: PSID, mask: u32, inherit: bool) -> Result<()> {
    let name = HSTRING::from(path);
    let inherit = inherit && path.is_dir();
    unsafe {
        let mut dacl: *mut ACL = null_mut();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        GetNamedSecurityInfoW(&name, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, None, None, Some(&mut dacl), None, &mut sd).ok()?;
        // ponytail: the ALL APPLICATION PACKAGES check is on the object only, trusting children inherit it.
        let r = if has_ace(dacl, sid, mask, inherit) || has_ace(dacl, all_app_packages(), mask, false) {
            Ok(())
        } else {
            let ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: mask,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: if inherit { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { NO_INHERITANCE },
                Trustee: TRUSTEE_W { TrusteeForm: TRUSTEE_IS_SID, ptstrName: PWSTR(sid.0 as _), ..Default::default() },
            };
            let mut new: *mut ACL = null_mut();
            let r = SetEntriesInAclW(Some(&[ea]), Some(dacl), &mut new).ok().and_then(|_| {
                if inherit {
                    SetNamedSecurityInfoW(&name, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, None, None, Some(new), None).ok()
                } else {
                    set_own_dacl(&name, sd, new)
                }
            });
            LocalFree(Some(HLOCAL(new as _)));
            r
        };
        LocalFree(Some(HLOCAL(sd.0)));
        r
    }
}

/// Replace only this object's DACL. SetNamedSecurityInfo would re-propagate inheritance to every
/// child, i.e. crawl the whole drive for `C:\` or the whole profile for `C:\Users\<me>`.
unsafe fn set_own_dacl(name: &HSTRING, old: PSECURITY_DESCRIPTOR, dacl: *const ACL) -> Result<()> {
    unsafe {
        let (mut ctl, mut rev) = (0u16, 0u32);
        GetSecurityDescriptorControl(old, &mut ctl, &mut rev)?;
        let mut sd = SECURITY_DESCRIPTOR::default();
        let p = PSECURITY_DESCRIPTOR(&mut sd as *mut _ as _);
        InitializeSecurityDescriptor(p, SECURITY_DESCRIPTOR_REVISION)?;
        SetSecurityDescriptorDacl(p, true, Some(dacl), false)?;
        let keep = SE_DACL_AUTO_INHERITED | SE_DACL_PROTECTED;
        SetSecurityDescriptorControl(p, keep, SECURITY_DESCRIPTOR_CONTROL(ctl) & keep)?;
        SetFileSecurityW(name, DACL_SECURITY_INFORMATION, p).ok()
    }
}

fn all_app_packages() -> PSID {
    let mut p = PSID::default();
    // S-1-15-2-1; leaked once per call, the shim is short-lived
    let _ = unsafe { ConvertStringSidToSidW(&HSTRING::from("S-1-15-2-1"), &mut p) };
    p
}

unsafe fn has_ace(acl: *const ACL, sid: PSID, mask: u32, dir: bool) -> bool {
    if acl.is_null() {
        return true; // null DACL: everyone has full access
    }
    let inherit = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE).0 as u8;
    unsafe {
        for i in 0..(*acl).AceCount as u32 {
            let mut ace = null_mut();
            if GetAce(acl, i, &mut ace).is_err() {
                continue;
            }
            let h = &*(ace as *const ACE_HEADER);
            if h.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            let a = &*(ace as *const ACCESS_ALLOWED_ACE);
            if a.Mask & mask == mask
                && (!dir || h.AceFlags & inherit == inherit)
                && EqualSid(PSID(&a.SidStart as *const u32 as _), sid).is_ok()
            {
                return true;
            }
        }
    }
    false
}
