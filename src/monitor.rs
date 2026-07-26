#[derive(Debug, Clone, PartialEq)]
pub struct MonInfo {
    pub index: usize,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
    pub device: String,
    pub label: String,
}

impl std::fmt::Display for MonInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

impl MonInfo {
    pub fn same_screen(&self, other: &MonInfo) -> bool {
        self.left == other.left
            && self.top == other.top
            && self.width == other.width
            && self.height == other.height
    }
}

pub fn enumerate() -> Vec<MonInfo> {
    enumerate_impl()
}

pub fn by_index(index: usize) -> Option<MonInfo> {
    enumerate().into_iter().find(|m| m.index == index)
}

pub fn default_nav_index() -> usize {
    enumerate()
        .into_iter()
        .find(|m| m.primary)
        .map(|m| m.index)
        .unwrap_or(0)
}

pub fn default_pad_index() -> usize {
    let all = enumerate();
    all.iter()
        .find(|m| !m.primary)
        .or_else(|| all.first())
        .map(|m| m.index)
        .unwrap_or(0)
}

pub fn resolve_index(stored: i32, auto: usize) -> usize {
    let all = enumerate();
    if stored >= 0 {
        let i = stored as usize;
        if all.iter().any(|m| m.index == i) {
            return i;
        }
    }
    auto.min(all.len().saturating_sub(1))
}

pub fn labels() -> Vec<String> {
    enumerate().into_iter().map(|m| m.label).collect()
}

pub fn set_flipped(device: &str, flipped: bool) -> Result<(), String> {
    set_flipped_impl(device, flipped)
}

#[cfg(not(windows))]
fn enumerate_impl() -> Vec<MonInfo> {
    Vec::new()
}

#[cfg(not(windows))]
fn set_flipped_impl(_device: &str, _flipped: bool) -> Result<(), String> {
    Err("display rotation is Windows-only".into())
}

#[cfg(windows)]
fn enumerate_impl() -> Vec<MonInfo> {
    use windows_sys::Win32::Graphics::Gdi::EnumDisplayMonitors;
    let mut monitors: Vec<MonInfo> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(enum_proc),
            &mut monitors as *mut Vec<MonInfo> as isize,
        );
    }
    for (i, m) in monitors.iter_mut().enumerate() {
        m.index = i;
        m.label = format!(
            "{i}: {}×{}{} @ {},{}",
            m.width,
            m.height,
            if m.primary { " primary" } else { "" },
            m.left,
            m.top
        );
    }
    monitors
}

#[cfg(windows)]
unsafe extern "system" fn enum_proc(
    hmon: windows_sys::Win32::Graphics::Gdi::HMONITOR,
    _hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    _clip: *mut windows_sys::Win32::Foundation::RECT,
    data: windows_sys::Win32::Foundation::LPARAM,
) -> i32 {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO, MONITORINFOEXW};
    use windows_sys::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

    let monitors = &mut *(data as *mut Vec<MonInfo>);
    let mut miex: MONITORINFOEXW = std::mem::zeroed();
    miex.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmon, &mut miex as *mut MONITORINFOEXW as *mut MONITORINFO) != 0 {
        let r = miex.monitorInfo.rcMonitor;
        let primary = miex.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
        let width = r.right - r.left;
        let height = r.bottom - r.top;
        let device = widestr_to_string(&miex.szDevice);
        monitors.push(MonInfo {
            index: 0,
            left: r.left,
            top: r.top,
            width,
            height,
            primary,
            device,
            label: String::new(),
        });
    }
    1
}

#[cfg(windows)]
fn widestr_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(windows)]
fn set_flipped_impl(device: &str, flipped: bool) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsExW, CDS_RESET, CDS_UPDATEREGISTRY, DEVMODEW,
        DISP_CHANGE_SUCCESSFUL, DMDO_180, DMDO_DEFAULT, DM_DISPLAYORIENTATION,
        ENUM_CURRENT_SETTINGS,
    };

    if device.is_empty() {
        return Err("monitor has no device name".into());
    }

    let want = if flipped { DMDO_180 } else { DMDO_DEFAULT };
    let mut name: Vec<u16> = std::ffi::OsStr::new(device)
        .encode_wide()
        .chain(Some(0))
        .collect();

    let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;

    let ok = unsafe { EnumDisplaySettingsExW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut dm, 0) };
    if ok == 0 {
        return Err(format!("EnumDisplaySettingsEx failed for {device}"));
    }

    let cur = unsafe { dm.Anonymous1.Anonymous2.dmDisplayOrientation };
    if cur == want {
        log::info!("{device} already orientation {want}");
        return Ok(());
    }

    dm.Anonymous1.Anonymous2.dmDisplayOrientation = want;
    dm.dmFields |= DM_DISPLAYORIENTATION;

    let rc = unsafe {
        ChangeDisplaySettingsExW(
            name.as_mut_ptr(),
            &dm,
            std::ptr::null_mut(),
            CDS_UPDATEREGISTRY | CDS_RESET,
            std::ptr::null(),
        )
    };
    if rc != DISP_CHANGE_SUCCESSFUL {
        return Err(format!(
            "ChangeDisplaySettingsEx({device} → {want}) failed: {rc}"
        ));
    }
    log::info!(
        "rotated {device} to {}",
        if flipped { "180°" } else { "0°" }
    );
    Ok(())
}
