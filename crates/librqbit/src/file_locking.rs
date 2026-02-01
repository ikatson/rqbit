use std::path::Path;

#[cfg(windows)]
pub fn kill_processes_locking_path(path: &Path, _recursive: bool) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{ERROR_MORE_DATA, WIN32_ERROR};
    use windows::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmShutdown, RmStartSession,
        RM_PROCESS_INFO,
    };

    fn check_win32(err: WIN32_ERROR) -> windows::core::Result<()> {
        if err.is_ok() {
            Ok(())
        } else {
            Err(windows::core::Error::from(err))
        }
    }

    let mut session_handle = 0;
    let mut session_key = [0u16; 32]; // CCH_RM_SESSION_KEY + 1

    unsafe {
        check_win32(RmStartSession(
            &mut session_handle,
            Some(0),
            windows::core::PWSTR(session_key.as_mut_ptr()),
        ))?;
    }

    // Ensure session is closed primarily
    struct SessionGuard(u32);
    impl Drop for SessionGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = RmEndSession(self.0);
            }
        }
    }
    let _guard = SessionGuard(session_handle);

    let path_str: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let paths = [windows::core::PCWSTR(path_str.as_ptr())];

    unsafe {
        check_win32(RmRegisterResources(
            session_handle,
            Some(&paths),
            None,
            None,
        ))?;
    }

    let mut reason = 0;
    let mut n_proc_info_needed = 0;
    let mut n_proc_info = 0;

    let res = unsafe {
        RmGetList(
            session_handle,
            &mut n_proc_info_needed,
            &mut n_proc_info,
            None,
            &mut reason,
        )
    };

    if res == ERROR_MORE_DATA {
        n_proc_info = n_proc_info_needed;
        let mut process_info = vec![RM_PROCESS_INFO::default(); n_proc_info as usize];

        unsafe {
            check_win32(RmGetList(
                session_handle,
                &mut n_proc_info_needed,
                &mut n_proc_info,
                Some(process_info.as_mut_ptr()),
                &mut reason,
            ))?;
        }

        if n_proc_info > 0 {
             tracing::warn!("Found {} processes locking {:?}. Shutting them down...", n_proc_info, path);
             unsafe {
                 check_win32(RmShutdown(session_handle, 1, None))?;
             }
             tracing::info!("Processes terminated successfully.");
        }
    } else if res.is_ok() {
        tracing::debug!("No locking processes found for {:?}", path);
    } else {
        check_win32(res)?;
    }

    Ok(())
}

#[cfg(not(windows))]
pub fn kill_processes_locking_path(_path: &Path, _recursive: bool) -> anyhow::Result<()> {
    anyhow::bail!("kill_processes_locking_path is only supported on Windows")
}
