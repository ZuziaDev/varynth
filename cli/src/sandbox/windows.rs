//! Windows write boundary for host tool processes.
//!
//! A child is spawned with a restricted version of the caller's own token
//! (`CreateRestrictedToken`, privileges stripped) whose integrity level is
//! reduced to Low for `workspace-write` and Untrusted for `read-only`. Every
//! medium-integrity object (the user profile, the repository, `%TEMP%`, the
//! registry) then rejects writes from the child with an access-denied error
//! raised by the kernel, while reads stay permitted (no-read-up is not set on
//! ordinary objects).
//!
//! Only the roots the mode allows are relabelled Low so the child can write
//! there: the workspace plus the private sandbox temp directory. The child is
//! placed in a Job Object with kill-on-close, so descendants cannot outlive
//! the shell and a timeout tears the whole tree down.
//!
//! Nothing here needs administrator rights: labelling paths the user owns
//! needs `WRITE_OWNER`, and `CreateProcessAsUserW` waives
//! `SE_ASSIGNPRIMARYTOKEN` when the token is a restricted version of the
//! caller's own primary token.

use std::ffi::{c_void, OsStr, OsString};
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::LazyLock;

use anyhow::{bail, Context, Result};
use windows_sys::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, CreateWellKnownSid, EqualSid, GetAce, GetSecurityDescriptorSacl,
    SetTokenInformation, TokenIntegrityLevel, WinLowLabelSid, ACL, DISABLE_MAX_PRIVILEGE,
    LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES,
    SYSTEM_MANDATORY_LABEL_ACE, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, WELL_KNOWN_SID_TYPE,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::SystemServices::{
    SE_GROUP_INTEGRITY, SYSTEM_MANDATORY_LABEL_ACE_TYPE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, ResumeThread,
    TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

use super::{strip_verbatim, SandboxMode, WriteRoot};

/// Mandatory label applied to the roots a child may write. The child itself
/// always runs at Low; `Medium` restores the normal label so a `read-only`
/// spawn cannot write a tree an earlier `workspace-write` run had lowered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    Low,
    Medium,
}

impl Label {
    fn sddl(self) -> &'static str {
        match self {
            Self::Low => LOW_LABEL_SDDL,
            Self::Medium => MEDIUM_LABEL_SDDL,
        }
    }
}

/// A live child behind the boundary. Dropping it terminates anything left in
/// the job, so an abandoned shell cannot leak outside the caller.
pub(crate) struct ConfinedChild {
    process: HANDLE,
    job: HANDLE,
    stdout: Option<std::fs::File>,
    stderr: Option<std::fs::File>,
    exit: Option<u32>,
}

impl ConfinedChild {
    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout
            .take()
            .map(|file| Box::new(file) as Box<dyn Read + Send>)
    }

    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr
            .take()
            .map(|file| Box::new(file) as Box<dyn Read + Send>)
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(code) = self.exit {
            return Ok(Some(ExitStatus::from_raw(code)));
        }
        match unsafe { WaitForSingleObject(self.process, 0) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                if unsafe { GetExitCodeProcess(self.process, &mut code) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                self.exit = Some(code);
                Ok(Some(ExitStatus::from_raw(code)))
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    #[cfg(test)]
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    pub(crate) fn kill_tree(&mut self) {
        // The job holds every descendant, including ones that detached from
        // the shell's pipes.
        unsafe {
            TerminateJobObject(self.job, 1);
            if self.exit.is_none() {
                let mut code = 0u32;
                if GetExitCodeProcess(self.process, &mut code) != 0 {
                    self.exit = Some(code);
                }
            }
        }
    }
}

impl Drop for ConfinedChild {
    fn drop(&mut self) {
        unsafe {
            if self.exit.is_none() {
                TerminateJobObject(self.job, 1);
            }
            CloseHandle(self.process);
            CloseHandle(self.job);
        }
    }
}

/// The restricted Low-integrity token is immutable once built, so one is
/// cached for the life of the process and reused by every spawn.
fn token() -> Result<HANDLE> {
    static TOKEN: LazyLock<Cache> = LazyLock::new(|| {
        build_token()
            .map(|handle| handle as usize)
            .map_err(|error| error.to_string())
    });
    TOKEN
        .as_ref()
        .map(|handle| *handle as HANDLE)
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

type Cache = std::result::Result<usize, String>;

fn build_token() -> Result<HANDLE> {
    let mut current: HANDLE = std::ptr::null_mut();
    let access = TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT;
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut current) } == 0 {
        bail!("OpenProcessToken failed: {}", io::Error::last_os_error());
    }
    let mut restricted: HANDLE = std::ptr::null_mut();
    let created = unsafe {
        CreateRestrictedToken(
            current,
            DISABLE_MAX_PRIVILEGE,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            &mut restricted,
        )
    };
    unsafe { CloseHandle(current) };
    if created == 0 {
        bail!(
            "CreateRestrictedToken failed: {}",
            io::Error::last_os_error()
        );
    }

    let sid = well_known_sid(WinLowLabelSid)?;
    let label = TOKEN_MANDATORY_LABEL {
        Label: SID_AND_ATTRIBUTES {
            Sid: sid.as_ptr() as *mut c_void,
            Attributes: SE_GROUP_INTEGRITY as u32,
        },
    };
    let set = unsafe {
        SetTokenInformation(
            restricted,
            TokenIntegrityLevel,
            &label as *const _ as *const c_void,
            std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32,
        )
    };
    if set == 0 {
        let error = io::Error::last_os_error();
        unsafe { CloseHandle(restricted) };
        bail!("SetTokenInformation(TokenIntegrityLevel) failed: {error}");
    }
    Ok(restricted)
}

fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<Vec<u8>> {
    // 68 bytes covers every well-known SID; round up for alignment slack.
    let mut buffer = vec![0u8; 128];
    let mut size = buffer.len() as u32;
    if unsafe {
        CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            buffer.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    } == 0
    {
        bail!("CreateWellKnownSid failed: {}", io::Error::last_os_error());
    }
    buffer.truncate(size as usize);
    Ok(buffer)
}

/// Mandatory labels with object+container inheritance: entries created in the
/// directory later inherit it, so new build outputs keep the same level.
const LOW_LABEL_SDDL: &str = "S:(ML;OICI;NW;;;LW)";
const MEDIUM_LABEL_SDDL: &str = "S:(ML;OICI;NW;;;ME)";

fn label_sacl(label: Label) -> Result<(*mut ACL, PSECURITY_DESCRIPTOR)> {
    let mut sddl: Vec<u16> = label.sddl().encode_utf16().collect();
    sddl.push(0);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        bail!(
            "failed to build the {:?} integrity label: {}",
            label,
            io::Error::last_os_error()
        );
    }
    let mut present = 0;
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorSacl(descriptor, &mut present, &mut sacl, &mut defaulted) }
        == 0
        || present == 0
    {
        unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor as _) };
        bail!("the {:?} integrity label has no SACL", label);
    }
    Ok((sacl, descriptor))
}

fn set_label(path: &Path, sacl: *mut ACL) -> Result<()> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            sacl,
        )
    };
    if status != 0 {
        bail!(
            "failed to set the integrity label on {}: {}",
            path.display(),
            io::Error::from_raw_os_error(status as i32)
        );
    }
    Ok(())
}

/// Whether `path` currently carries the Low mandatory label. Read from the
/// object itself, not from memory, so a label changed by another Varynth
/// process (or by a previous run) is always noticed.
pub(super) fn is_low(path: &Path) -> bool {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sacl,
            &mut descriptor,
        )
    };
    if status != 0 || sacl.is_null() {
        return false;
    }
    let mut found = false;
    if let Ok(low) = well_known_sid(WinLowLabelSid) {
        let count = unsafe { (*sacl).AceCount };
        for index in 0..count {
            let mut ace: *mut c_void = std::ptr::null_mut();
            if unsafe { GetAce(sacl, index as u32, &mut ace) } == 0 || ace.is_null() {
                break;
            }
            let ace = ace as *const SYSTEM_MANDATORY_LABEL_ACE;
            if unsafe { (*ace).Header.AceType } as u32 == SYSTEM_MANDATORY_LABEL_ACE_TYPE
                && unsafe {
                    EqualSid(
                        std::ptr::addr_of!((*ace).SidStart) as *mut c_void,
                        low.as_ptr() as *mut c_void,
                    )
                } != 0
            {
                found = true;
                break;
            }
        }
    }
    unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor as _) };
    found
}

/// Walk `root` and set `label` on every entry. Used only when the root's
/// current label disagrees, so repeated spawns cost one check per root.
fn apply_level(root: &Path, label: Label, recursive: bool) -> Result<()> {
    let (sacl, descriptor) = label_sacl(label)?;
    let result = (|| -> Result<()> {
        if recursive {
            for entry in walkdir::WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| entry.ok())
            {
                // Files owned elsewhere or on odd volumes keep their label;
                // the root itself is mandatory.
                let _ = set_label(entry.path(), sacl);
            }
        }
        set_label(root, sacl)
            .with_context(|| format!("cannot relabel the workspace at {}", root.display()))
    })();
    unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor as _) };
    result
}

fn prepare_roots(roots: &[WriteRoot], level: Label) -> Result<()> {
    for root in roots {
        let canonical = strip_verbatim(
            root.path
                .canonicalize()
                .unwrap_or_else(|_| root.path.clone()),
        );
        let already_low = is_low(&canonical);
        if already_low == (level == Label::Low) {
            continue;
        }
        apply_level(&canonical, level, root.recursive)?;
    }
    Ok(())
}

/// Private temp directory handed to the child through `TMP`/`TEMP`. It lives
/// under the user's temp folder but is the only temp path the child can write.
fn sandbox_temp() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("varynth-sandbox-tmp");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let meta = std::fs::symlink_metadata(&dir)?;
    if meta.file_type().is_symlink() {
        bail!(
            "refusing to use the sandbox temp directory {} because it is a link",
            dir.display()
        );
    }
    Ok(dir)
}

fn environment_block(temp: &Path) -> Vec<u16> {
    // Only the allowlisted variables cross into the child; secrets such as
    // VARYNTH_*TOKEN*, provider keys and the rest of the parent environment
    // stay behind.
    let mut vars: Vec<(OsString, OsString)> = super::child_env();
    let value = temp.as_os_str().to_os_string();
    for key in ["TMP", "TEMP", "TMPDIR"] {
        match vars
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
        {
            Some(slot) => slot.1 = value.clone(),
            None => vars.push((OsString::from(key), value.clone())),
        }
    }
    vars.sort_by_key(|(name, _)| name.to_string_lossy().to_ascii_lowercase());
    let mut block = Vec::new();
    for (key, val) in vars {
        block.extend(key.encode_wide());
        block.push('=' as u16);
        block.extend(val.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

fn quote_arg(arg: &OsStr, out: &mut Vec<u16>) {
    let raw: Vec<u16> = arg.encode_wide().collect();
    let plain = !raw.is_empty() && !raw.iter().any(|c| matches!(*c, 0x20 | 0x09 | 0x22));
    if plain {
        out.extend(raw);
        return;
    }
    out.push('"' as u16);
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == '\\' as u16 {
            let mut slashes = 0;
            while index < raw.len() && raw[index] == '\\' as u16 {
                slashes += 1;
                index += 1;
            }
            if index == raw.len() {
                for _ in 0..slashes * 2 {
                    out.push('\\' as u16);
                }
            } else if raw[index] == '"' as u16 {
                for _ in 0..slashes * 2 + 1 {
                    out.push('\\' as u16);
                }
                out.push('"' as u16);
                index += 1;
            } else {
                for _ in 0..slashes {
                    out.push('\\' as u16);
                }
            }
        } else {
            out.push(raw[index]);
            index += 1;
        }
    }
    out.push('"' as u16);
}

fn command_line(program: &str, args: &[String]) -> Vec<u16> {
    let mut line = Vec::new();
    quote_arg(OsStr::new(program), &mut line);
    for arg in args {
        line.push(' ' as u16);
        quote_arg(OsStr::new(arg), &mut line);
    }
    line.push(0);
    line
}

fn wide_path(path: &Path) -> Vec<u16> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide
}

fn inheritable_pipe() -> io::Result<(HANDLE, HANDLE)> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read: HANDLE = INVALID_HANDLE_VALUE;
    let mut write: HANDLE = INVALID_HANDLE_VALUE;
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Only the write end crosses into the child.
    unsafe { windows_sys::Win32::Foundation::SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0) };
    Ok((read, write))
}

/// Spawn `program args` in `cwd` behind the write boundary.
pub(crate) fn spawn(
    program: &str,
    args: &[String],
    cwd: &Path,
    roots: &[WriteRoot],
    mode: SandboxMode,
) -> Result<ConfinedChild> {
    let token = token()?;
    let temp = sandbox_temp()?;
    // workspace-write lowers the workspace and the private temp root so the
    // child can write there; read-only raises both back to Medium so a tree
    // an earlier workspace-write run lowered is not writable either.
    let level = match mode {
        SandboxMode::WorkspaceWrite => Label::Low,
        _ => Label::Medium,
    };
    prepare_roots(roots, level)?;
    let (sacl, descriptor) = label_sacl(level)?;
    let labelled = set_label(&temp, sacl);
    unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor as _) };
    labelled?;

    let (out_read, out_write) = inheritable_pipe()?;
    let (err_read, err_write) = match inheritable_pipe() {
        Ok(pair) => pair,
        Err(error) => {
            unsafe {
                CloseHandle(out_read);
                CloseHandle(out_write);
            }
            return Err(error.into());
        }
    };

    // stdin is NUL: the shell must never block on console input.
    let null_in = std::fs::OpenOptions::new().read(true).open("NUL")?;
    unsafe {
        windows_sys::Win32::Foundation::SetHandleInformation(
            null_in.as_raw_handle() as HANDLE,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    };

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        unsafe {
            CloseHandle(out_read);
            CloseHandle(out_write);
            CloseHandle(err_read);
            CloseHandle(err_write);
        }
        bail!("CreateJobObject failed: {}", io::Error::last_os_error());
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
            CloseHandle(out_read);
            CloseHandle(out_write);
            CloseHandle(err_read);
            CloseHandle(err_write);
        }
        bail!("SetInformationJobObject failed: {error}");
    }

    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTF_USESTDHANDLES;
    startup.hStdInput = null_in.as_raw_handle() as HANDLE;
    startup.hStdOutput = out_write;
    startup.hStdError = err_write;

    // The executable is always an absolute path: `lpApplicationName` is set
    // explicitly so Windows never applies its search order (which includes
    // the parent's current directory) to a bare name.
    let program_path = Path::new(program);
    if !program_path.is_absolute() {
        unsafe {
            CloseHandle(job);
            CloseHandle(out_read);
            CloseHandle(err_read);
        }
        bail!(
            "refusing to start `{program}` under the sandbox: the executable must be an absolute path"
        );
    }
    let environment = environment_block(&temp);
    let mut line = command_line(program, args);
    let application = wide_path(program_path);
    let cwd_wide = wide_path(cwd);
    let mut process_info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let created = unsafe {
        CreateProcessAsUserW(
            token,
            application.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr() as *const c_void,
            cwd_wide.as_ptr(),
            &startup,
            &mut process_info,
        )
    };
    // The child owns its ends now.
    unsafe {
        CloseHandle(out_write);
        CloseHandle(err_write);
    }
    drop(null_in);
    if created == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
            CloseHandle(out_read);
            CloseHandle(err_read);
        }
        bail!(
            "cannot start `{program}` under the sandbox token ({error}); \
             the OS boundary is required in this mode — use --sandbox danger-full-access to opt out"
        );
    }

    if unsafe { AssignProcessToJobObject(job, process_info.hProcess) } == 0 {
        let error = io::Error::last_os_error();
        unsafe {
            TerminateProcess(process_info.hProcess, 1);
            CloseHandle(process_info.hThread);
            CloseHandle(process_info.hProcess);
            CloseHandle(job);
            CloseHandle(out_read);
            CloseHandle(err_read);
        }
        bail!("cannot confine the shell in a Job Object: {error}");
    }

    unsafe {
        ResumeThread(process_info.hThread);
        CloseHandle(process_info.hThread);
    }

    Ok(ConfinedChild {
        process: process_info.hProcess,
        job,
        stdout: Some(unsafe { std::fs::File::from_raw_handle(out_read as RawHandle) }),
        stderr: Some(unsafe { std::fs::File::from_raw_handle(err_read as RawHandle) }),
        exit: None,
    })
}

/// Cheap capability check used by `doctor`: build the restricted token, a job
/// object, and both labels, then discard them. No child process is started.
pub(crate) fn probe() -> Result<()> {
    token()?;
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        bail!("CreateJobObject failed: {}", io::Error::last_os_error());
    }
    unsafe { CloseHandle(job) };
    let dir = std::env::temp_dir().join("varynth-sandbox-probe");
    std::fs::create_dir_all(&dir)?;
    let mut failure = None;
    for level in [Label::Low, Label::Medium] {
        let (sacl, descriptor) = label_sacl(level)?;
        let applied = set_label(&dir, sacl);
        unsafe { windows_sys::Win32::Foundation::LocalFree(descriptor as _) };
        if let Err(error) = applied {
            failure = Some(error);
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
