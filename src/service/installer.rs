use std::ffi::OsString;
use std::path::PathBuf;
use windows_service::{
    service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};

const SERVICE_NAME: &str = "xbridge";
const INSTALL_EXE: &str = r"C:\windows\xbridge.exe";

fn service_info(start_type: ServiceStartType) -> ServiceInfo {
    ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("xbridge Discord RPC Bridge"),
        service_type: ServiceType::OWN_PROCESS,
        start_type,
        error_control: ServiceErrorControl::Normal,
        executable_path: PathBuf::from(INSTALL_EXE),
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    }
}

pub fn install() -> anyhow::Result<()> {
    let mgr = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CREATE_SERVICE | ServiceManagerAccess::CONNECT,
    )?;

    // Already installed — silently skip, just ensure it's enabled and running.
    if mgr.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS).is_ok() {
        crate::log!("[install] already installed, ensuring enabled.");
        return enable();
    }

    let src = std::env::current_exe()?;
    std::fs::copy(&src, PathBuf::from(INSTALL_EXE))?;

    crate::log!("[install] downloading detectable list...");
    crate::discovery::list_sync::download_fresh()?;

    let svc = mgr.create_service(
        &service_info(ServiceStartType::AutoStart),
        ServiceAccess::START,
    )?;
    svc.start::<&str>(&[])?;

    crate::log!("[install] xbridge installed and started.");
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    use windows_service::service::ServiceState;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};

    let mgr = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )?;
    let svc = mgr.open_service(
        SERVICE_NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    )?;

    // Stop if running
    if let Ok(status) = svc.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = svc.stop();
            // Wait up to 10s for the service to stop
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if let Ok(s) = svc.query_status() {
                    if s.current_state == ServiceState::Stopped {
                        break;
                    }
                }
            }
        }
    }

    svc.delete()?;

    // Schedule exe deletion at next reboot (can't delete a running binary)
    let exe_wide: Vec<u16> = INSTALL_EXE.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MoveFileExW(exe_wide.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT);
    }

    crate::log!("[uninstall] xbridge uninstalled. Exe will be deleted on next reboot.");
    Ok(())
}

pub fn enable() -> anyhow::Result<()> {
    use windows_service::service::ServiceState;

    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let svc = mgr.open_service(
        SERVICE_NAME,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::QUERY_STATUS,
    )?;
    svc.change_config(&service_info(ServiceStartType::AutoStart))?;
    if svc.query_status()?.current_state == ServiceState::Stopped {
        svc.start::<&str>(&[])?;
    }
    crate::log!("[enable] xbridge enabled and started.");
    Ok(())
}

pub fn disable() -> anyhow::Result<()> {
    use windows_service::service::ServiceState;

    let mgr = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT,
    )?;
    let svc = mgr.open_service(
        SERVICE_NAME,
        ServiceAccess::CHANGE_CONFIG | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
    )?;

    if let Ok(status) = svc.query_status() {
        if status.current_state != ServiceState::Stopped {
            let _ = svc.stop();
        }
    }
    svc.change_config(&service_info(ServiceStartType::Disabled))?;
    crate::log!("[disable] xbridge disabled.");
    Ok(())
}
