//! Platform backends that keep an API key out of a command line.
//!
//! `api_key_command` is stored in the config file and replayed later, so it
//! must never carry the secret: `argv` is readable by every process of the
//! same user (`ps`, WMI `Win32_Process.CommandLine`). Linux already feeds
//! `secret-tool` through stdin; macOS writes through the Security framework
//! and Windows through `CredWriteW` with the secret piped into PowerShell.

/// The `Add-Type` C# shim shared by the Windows read and write scripts.
///
/// It is embedded in a PowerShell single-quoted string, so it must not contain
/// a single quote or a newline.
#[cfg(any(target_os = "windows", test))]
const WINDOWS_CREDENTIAL_SHIM: &str = "Add-Type -TypeDefinition 'using System;using System.Runtime.InteropServices;\
     public class QCred{[DllImport(\"Advapi32.dll\",EntryPoint=\"CredReadW\",CharSet=CharSet.Unicode)]\
     public static extern bool CredRead(string t,int ty,int fl,out IntPtr p);\
     [DllImport(\"Advapi32.dll\",EntryPoint=\"CredWriteW\",CharSet=CharSet.Unicode)]\
     public static extern bool CredWrite(ref C c,int fl);\
     [DllImport(\"Advapi32.dll\",EntryPoint=\"CredDeleteW\",CharSet=CharSet.Unicode)]\
     public static extern bool CredDelete(string t,int ty,int fl);\
     [StructLayout(LayoutKind.Sequential)]public struct C{public int Flags;public int Type;\
     public IntPtr TargetName;public IntPtr Comment;public long LastWritten;\
     public int CredentialBlobSize;public IntPtr CredentialBlob;public int Persist;\
     public int AttributeCount;public IntPtr Attributes;public IntPtr TargetAlias;\
     public IntPtr UserName;}\
     [DllImport(\"Advapi32.dll\")]static extern void CredFree(IntPtr p);\
     public static string Get(string t){IntPtr p;if(!CredRead(t,1,0,out p))return null;\
     var c=(C)Marshal.PtrToStructure(p,typeof(C));if(c.CredentialBlobSize==0){CredFree(p);return \"\";}\
     var b=new byte[c.CredentialBlobSize];Marshal.Copy(c.CredentialBlob,b,0,c.CredentialBlobSize);\
     CredFree(p);return System.Text.Encoding.Unicode.GetString(b);}\
     public static bool Set(string t,string u,string s){var b=System.Text.Encoding.Unicode.GetBytes(s);\
     var c=new C();c.Type=1;c.Persist=2;c.TargetName=Marshal.StringToCoTaskMemUni(t);\
     c.UserName=Marshal.StringToCoTaskMemUni(u);c.CredentialBlob=Marshal.AllocCoTaskMem(b.Length);\
     Marshal.Copy(b,0,c.CredentialBlob,b.Length);c.CredentialBlobSize=b.Length;\
     bool ok=CredWrite(ref c,0);Marshal.FreeCoTaskMem(c.TargetName);Marshal.FreeCoTaskMem(c.UserName);\
     Marshal.FreeCoTaskMem(c.CredentialBlob);return ok;}\
     public static bool Del(string t){return CredDelete(t,1,0);}\
     }';";

/// Renders `value` as a PowerShell single-quoted literal, doubling embedded
/// quotes so a provider name cannot break out of the string.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn ps_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// One-line PowerShell command that prints a generic credential's blob to
/// stdout (the API key).
///
/// It lands verbatim in `api_key_command` — one argv per line — so it must
/// contain no newline; the kernel runs `powershell -NoProfile -Command
/// <script>` and reads the output as the key.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn windows_credential_read_script(target: &str) -> String {
    format!(
        "{WINDOWS_CREDENTIAL_SHIM} [QCred]::Get({})",
        ps_single_quoted(target)
    )
}

/// One-line PowerShell command that stores `user`'s secret for `target`
/// in the generic credential store, reading the secret from stdin.
///
/// Nothing secret appears in the command line: the caller pipes the key into
/// the child's stdin and the script trims it with `.Trim()`.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn windows_credential_write_script(target: &str, user: &str) -> String {
    format!(
        "{WINDOWS_CREDENTIAL_SHIM} $v=[Console]::In.ReadToEnd();\
         $ok=[QCred]::Set({},{},$v.Trim());exit ([int](-not $ok))",
        ps_single_quoted(target),
        ps_single_quoted(user),
    )
}

/// One-line PowerShell command that drops the generic credential written by
/// [`windows_credential_write_script`].
///
/// Nothing calls it yet: the round-trip test uses it as the cleanup step, and
/// it stays here so a future "forget key" action has a matching delete path.
#[cfg(any(target_os = "windows", test))]
#[allow(dead_code)]
pub(crate) fn windows_credential_delete_script(target: &str) -> String {
    format!(
        "{WINDOWS_CREDENTIAL_SHIM} [QCred]::Del({}) | Out-Null",
        ps_single_quoted(target)
    )
}

/// Keychain access on macOS through the Security framework.
///
/// The `security` CLI can only take the password as an argument (`-w <key>`),
/// which leaks it into `ps`; the C API has no such limit.
#[cfg(target_os = "macos")]
pub(crate) mod macos {
    use std::ffi::{c_char, c_void};

    const ERR_SEC_SUCCESS: i32 = 0;
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
    const ERR_SEC_DUPLICATE_ITEM: i32 = -25299;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        #[cfg(test)]
        fn SecKeychainCopyDefault(keychain: *mut *mut c_void) -> i32;
        fn SecKeychainAddGenericPassword(
            keychain: *const c_void,
            service_name_length: u32,
            service_name: *const c_char,
            account_name_length: u32,
            account_name: *const c_char,
            password_length: u32,
            password_data: *const c_void,
            item_ref: *mut *mut c_void,
        ) -> i32;
        fn SecKeychainFindGenericPassword(
            keychain_or_array: *const c_void,
            service_name_length: u32,
            service_name: *const c_char,
            account_name_length: u32,
            account_name: *const c_char,
            password_length: *mut u32,
            password_data: *mut *mut c_void,
            item_ref: *mut *mut c_void,
        ) -> i32;
        fn SecKeychainItemModifyAttributesAndData(
            item_ref: *mut c_void,
            attr_list: *const c_void,
            length: u32,
            data: *const c_void,
        ) -> i32;
        #[cfg(test)]
        fn SecKeychainItemDelete(item_ref: *mut c_void) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    fn len(value: &str) -> u32 {
        value.len() as u32
    }

    /// Stores (or overwrites) a generic password in the default keychain.
    /// On failure the `OSStatus` code is returned.
    pub(crate) fn store(service: &str, account: &str, secret: &str) -> Result<(), i32> {
        let status = unsafe {
            SecKeychainAddGenericPassword(
                std::ptr::null(),
                len(service),
                service.as_ptr().cast::<c_char>(),
                len(account),
                account.as_ptr().cast::<c_char>(),
                len(secret),
                secret.as_ptr().cast::<c_void>(),
                std::ptr::null_mut(),
            )
        };
        match status {
            ERR_SEC_SUCCESS => Ok(()),
            // `-U` on the CLI: overwrite the existing item instead of failing.
            ERR_SEC_DUPLICATE_ITEM => {
                let mut item: *mut c_void = std::ptr::null_mut();
                let status = unsafe {
                    SecKeychainFindGenericPassword(
                        std::ptr::null(),
                        len(service),
                        service.as_ptr().cast::<c_char>(),
                        len(account),
                        account.as_ptr().cast::<c_char>(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut item,
                    )
                };
                if status != ERR_SEC_SUCCESS {
                    return Err(status);
                }
                let status = unsafe {
                    SecKeychainItemModifyAttributesAndData(
                        item,
                        std::ptr::null(),
                        len(secret),
                        secret.as_ptr().cast::<c_void>(),
                    )
                };
                unsafe { CFRelease(item) };
                if status == ERR_SEC_SUCCESS {
                    Ok(())
                } else {
                    Err(status)
                }
            }
            other => Err(other),
        }
    }

    /// Removes a generic password; a missing item counts as success. Only the
    /// round-trip test needs it today.
    #[cfg(test)]
    pub(crate) fn delete(service: &str, account: &str) -> Result<(), i32> {
        let mut item: *mut c_void = std::ptr::null_mut();
        let status = unsafe {
            SecKeychainFindGenericPassword(
                std::ptr::null(),
                len(service),
                service.as_ptr().cast::<c_char>(),
                len(account),
                account.as_ptr().cast::<c_char>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut item,
            )
        };
        match status {
            ERR_SEC_SUCCESS => {
                let status = unsafe { SecKeychainItemDelete(item) };
                unsafe { CFRelease(item) };
                if status == ERR_SEC_SUCCESS {
                    Ok(())
                } else {
                    Err(status)
                }
            }
            ERR_SEC_ITEM_NOT_FOUND => Ok(()),
            other => Err(other),
        }
    }

    /// Whether the process can reach a default keychain at all; headless or
    /// locked sessions cannot, and callers should then fall back to an
    /// environment variable instead of blaming the key.
    #[cfg(test)]
    pub(crate) fn available() -> bool {
        let mut keychain: *mut c_void = std::ptr::null_mut();
        let status = unsafe { SecKeychainCopyDefault(&mut keychain) };
        if !keychain.is_null() {
            unsafe { CFRelease(keychain) };
        }
        status == ERR_SEC_SUCCESS
    }

    /// Human-readable name for the status codes we expect to see.
    pub(crate) fn describe(status: i32) -> String {
        match status {
            -25293 => "errSecAuthFailed".to_string(),
            -25294 => "errSecNoSuchKeychain".to_string(),
            ERR_SEC_DUPLICATE_ITEM => "errSecDuplicateItem".to_string(),
            ERR_SEC_ITEM_NOT_FOUND => "errSecItemNotFound".to_string(),
            -25308 => "errSecInteractionNotAllowed".to_string(),
            other => format!("OSStatus {other}"),
        }
    }
}
