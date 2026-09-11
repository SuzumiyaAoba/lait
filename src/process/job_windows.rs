//! Windows Job Object plumbing for `CommandProcessTree`: a spawned command
//! starts suspended (`configure`), gets assigned to a private Job Object
//! before its primary thread is resumed (`attach`) so a shell cannot spawn a
//! descendant in the gap between `CreateProcess` and
//! `AssignProcessToJobObject`, and the whole tree is torn down in one call
//! (`terminate`) rather than by walking descendants individually.
//!
//! Moved out of `process.rs` verbatim (no logic changed) — the
//! `x86_64-pc-windows-msvc` target isn't installed in this environment, so
//! this file was relocated without a compile check on Windows itself; the
//! `ci.yml` `native-locks` job only runs `file_lock::`/`async_io::`-filtered
//! tests on Windows, so a mistake here would stay invisible until a tagged
//! release build (`release.yml`).

use std::{
    ffi::c_void,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    ptr,
};

type Bool = i32;

const CREATE_SUSPENDED: u32 = 0x0000_0004;
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
const THREAD_SUSPEND_RESUME: u32 = 0x0000_0002;
const INVALID_HANDLE_VALUE: RawHandle = -1isize as RawHandle;
const INVALID_RESUME_COUNT: u32 = u32::MAX;

#[repr(C)]
struct ThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "CreateJobObjectW"]
    fn create_job_object_w(lp_job_attributes: *mut c_void, lp_name: *const u16) -> RawHandle;
    #[link_name = "AssignProcessToJobObject"]
    fn assign_process_to_job_object(job: RawHandle, process: RawHandle) -> Bool;
    #[link_name = "TerminateJobObject"]
    fn terminate_job_object(job: RawHandle, exit_code: u32) -> Bool;
    #[link_name = "CreateToolhelp32Snapshot"]
    fn create_toolhelp32_snapshot(flags: u32, process_id: u32) -> RawHandle;
    #[link_name = "Thread32First"]
    fn thread32_first(snapshot: RawHandle, entry: *mut ThreadEntry32) -> Bool;
    #[link_name = "Thread32Next"]
    fn thread32_next(snapshot: RawHandle, entry: *mut ThreadEntry32) -> Bool;
    #[link_name = "OpenThread"]
    fn open_thread(access: u32, inherit_handle: Bool, thread_id: u32) -> RawHandle;
    #[link_name = "ResumeThread"]
    fn resume_thread(thread: RawHandle) -> u32;
    #[link_name = "CloseHandle"]
    fn close_handle(handle: RawHandle) -> Bool;
    #[link_name = "GetProcessId"]
    fn get_process_id(process: RawHandle) -> u32;
}

pub(super) fn configure(command: &mut tokio::process::Command) {
    // Keep the primary thread stopped until the process has been assigned
    // to our Job Object.  Without this, a shell can create descendants in
    // the interval between CreateProcess and AssignProcessToJobObject;
    // those descendants would not inherit the job and would survive a
    // later timeout.
    command.creation_flags(CREATE_SUSPENDED);
}

fn resume_process(process: RawHandle) -> io::Result<()> {
    let process_id = unsafe { get_process_id(process) };
    if process_id == 0 {
        return Err(io::Error::last_os_error());
    }

    let snapshot = unsafe { create_toolhelp32_snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    let result = (|| {
        let mut entry = ThreadEntry32 {
            size: std::mem::size_of::<ThreadEntry32>() as u32,
            usage: 0,
            thread_id: 0,
            owner_process_id: 0,
            base_priority: 0,
            delta_priority: 0,
            flags: 0,
        };
        let mut found_thread = false;
        let mut has_entry = unsafe { thread32_first(snapshot, &mut entry) } != 0;
        while has_entry {
            if entry.owner_process_id == process_id {
                found_thread = true;
                let thread = unsafe { open_thread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let resume_result = unsafe { resume_thread(thread) };
                let close_result = unsafe { close_handle(thread) };
                if resume_result == INVALID_RESUME_COUNT {
                    return Err(io::Error::last_os_error());
                }
                if close_result == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            has_entry = unsafe { thread32_next(snapshot, &mut entry) } != 0;
        }

        if !found_thread {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "suspended command thread was not found",
            ));
        }
        Ok(())
    })();
    let close_result = unsafe { close_handle(snapshot) };
    if result.is_ok() && close_result == 0 {
        return Err(io::Error::last_os_error());
    }
    result
}

pub(super) fn attach(process: RawHandle) -> io::Result<OwnedHandle> {
    // A private, unnamed job has no ambient permissions or namespace
    // concerns. The handle remains owned by CommandProcessTree until the
    // command completes or cancellation cleanup runs.
    let job = unsafe { create_job_object_w(ptr::null_mut(), ptr::null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateJobObjectW returned a newly-owned kernel handle.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    let result = unsafe { assign_process_to_job_object(job.as_raw_handle(), process) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    if let Err(error) = resume_process(process) {
        // The process is still suspended if resuming failed.  Terminate
        // the Job before returning so a partially resumed process (or any
        // descendant it managed to create) cannot escape this failed
        // attach path.  The caller also kills/reaps the direct child.
        let _ = unsafe { terminate_job_object(job.as_raw_handle(), 1) };
        return Err(error);
    }
    Ok(job)
}

pub(super) fn terminate(job: RawHandle) -> io::Result<()> {
    let result = unsafe { terminate_job_object(job, 1) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
