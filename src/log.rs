use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

fn file() -> &'static Mutex<std::fs::File> {
    FILE.get_or_init(|| {
        let _ = std::fs::create_dir_all(r"C:\windows\logs");
        let f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(r"C:\windows\logs\xbridge.log")
            .expect("cannot open xbridge.log");
        Mutex::new(f)
    })
}

#[cfg(windows)]
fn timestamp() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetSystemTime;
    unsafe {
        let mut t = std::mem::zeroed::<SYSTEMTIME>();
        GetSystemTime(&mut t);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        )
    }
}

#[cfg(not(windows))]
fn timestamp() -> String {
    String::from("0000-00-00 00:00:00")
}

pub fn write(msg: &str) {
    let line = format!("[{}] {}\n", timestamp(), msg);
    if let Ok(mut f) = file().lock() {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::log::write(&::std::format!($($arg)*))
    };
}
