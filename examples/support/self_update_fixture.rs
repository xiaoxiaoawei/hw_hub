//! Native helpers used only by the disposable self-update smoke executable.
use anyhow::{Context, Result, ensure};
use std::{fs, io::Write, os::windows::ffi::OsStrExt, path::Path};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
    System::{
        LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW},
        Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
        },
    },
};

/// Set a genuine VERSIONINFO on a private copy, without changing package
/// versions or relaxing the production updater's version checks.
pub fn make_future_candidate(path: &Path, version: (u16, u16, u16)) -> Result<()> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let update = unsafe { BeginUpdateResourceW(wide.as_ptr(), 0) };
    ensure!(
        !update.is_null(),
        "BeginUpdateResourceW: {}",
        std::io::Error::last_os_error()
    );
    struct ResourceUpdate(HANDLE);
    impl Drop for ResourceUpdate {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { EndUpdateResourceW(self.0, 1) };
            }
        }
    }
    let mut update = ResourceUpdate(update);
    // VS_VERSION_INFO header, UTF-16 key, DWORD alignment, VS_FIXEDFILEINFO.
    let mut resource = Vec::with_capacity(92);
    for word in [92_u16, 52, 0] {
        resource.extend_from_slice(&word.to_le_bytes());
    }
    for word in "VS_VERSION_INFO\0".encode_utf16() {
        resource.extend_from_slice(&word.to_le_bytes());
    }
    resource.extend_from_slice(&[0, 0]);
    let ms = (u32::from(version.0) << 16) | u32::from(version.1);
    let ls = u32::from(version.2) << 16;
    for value in [
        0xfeef04bd_u32,
        0x00010000,
        ms,
        ls,
        ms,
        ls,
        0x3f,
        0,
        0x00040004,
        1,
        0,
        0,
        0,
    ] {
        resource.extend_from_slice(&value.to_le_bytes());
    }
    ensure!(resource.len() == 92, "Invalid smoke VERSIONINFO size");
    // winresource uses neutral language and version resource ID 1. Preserve
    // every other resource, including the executable's icon.
    ensure!(
        unsafe {
            UpdateResourceW(
                update.0,
                16_usize as _,
                1_usize as _,
                0,
                resource.as_ptr().cast(),
                resource.len() as u32,
            )
        } != 0,
        "UpdateResourceW: {}",
        std::io::Error::last_os_error()
    );
    let handle = std::mem::replace(&mut update.0, std::ptr::null_mut());
    ensure!(
        unsafe { EndUpdateResourceW(handle, 0) } != 0,
        "EndUpdateResourceW: {}",
        std::io::Error::last_os_error()
    );
    fs::OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(b"\nFlashTool isolated update smoke candidate\n")?;
    Ok(())
}

/// Own a handle only after proving this PID is our exact disposable image.
/// An open handle also prevents PID reuse from targeting an unrelated process.
pub struct OwnedFixtureProcess {
    handle: HANDLE,
    authenticated: bool,
}
impl OwnedFixtureProcess {
    pub fn open(pid: u32, expected_image: &Path) -> Result<Self> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        ensure!(
            !handle.is_null(),
            "Cannot open owned fixture process {pid}: {}",
            std::io::Error::last_os_error()
        );
        let mut process = Self {
            handle,
            authenticated: false,
        };
        let mut image = vec![0_u16; 32_768];
        let mut length = image.len() as u32;
        ensure!(
            unsafe { QueryFullProcessImageNameW(handle, 0, image.as_mut_ptr(), &mut length) } != 0,
            "Cannot identify owned fixture process"
        );
        use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};
        let actual = PathBuf::from(OsString::from_wide(&image[..length as usize]));
        ensure!(
            actual
                .canonicalize()
                .context("Resolve fixture process image")?
                == expected_image.canonicalize()?,
            "Fixture PID belongs to a different executable"
        );
        process.authenticated = true;
        Ok(process)
    }

    pub fn has_exited(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle, 0) == WAIT_OBJECT_0 }
    }
}
impl Drop for OwnedFixtureProcess {
    fn drop(&mut self) {
        if self.authenticated && !self.has_exited() {
            // This handle was authenticated against the temporary target path.
            unsafe {
                TerminateProcess(self.handle, 1);
                WaitForSingleObject(self.handle, 5_000);
            }
        }
        unsafe { CloseHandle(self.handle) };
    }
}
