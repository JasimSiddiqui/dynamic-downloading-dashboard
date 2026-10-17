use std::sync::OnceLock;

pub fn hide(cmd: &mut tokio::process::Command) {
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
}

#[cfg(windows)]
pub fn tie(child: &tokio::process::Child) {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    static JOB: OnceLock<usize> = OnceLock::new();
    let job = *JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        job as usize
    });
    if let (true, Some(h)) = (job != 0, child.raw_handle()) {
        unsafe {
            AssignProcessToJobObject(job as HANDLE, h as HANDLE);
        }
    }
}

#[cfg(not(windows))]
pub fn tie(_child: &tokio::process::Child) {}

pub fn spawn(cmd: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    hide(cmd);
    let child = cmd.spawn()?;
    tie(&child);
    Ok(child)
}

#[cfg(windows)]
pub fn awake(on: bool) {
    use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
    static TX: OnceLock<std::sync::mpsc::Sender<bool>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        std::thread::spawn(move || {
            for on in rx {
                unsafe {
                    SetThreadExecutionState(if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS });
                }
            }
        });
        tx
    });
    let _ = tx.send(on);
}

#[cfg(not(windows))]
pub fn awake(_on: bool) {}
