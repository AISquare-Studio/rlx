//! This host's name, as Python's `socket.gethostname()` says it, so that a registry record
//! rlxd writes names the host Python's records name (`aisquare.core.registry.host_name`).
#![allow(unsafe_code)]

/// The name, or `localhost` when the OS gives none, as `core.registry.host_name` falls
/// back.
pub fn name() -> String {
    os_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

/// `gethostname(2)`.
#[cfg(unix)]
fn os_name() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: `buffer` is writable for its whole length.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return None;
    }
    let len = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..len]).into_owned())
}

/// `GetComputerNameExW(ComputerNamePhysicalDnsHostname)`, which CPython's
/// `socket.gethostname()` calls on Windows.
#[cfg(windows)]
fn os_name() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, GetComputerNameExW,
    };
    let mut len = 0u32;
    // SAFETY: asking for the size only; this call fails by design and sets `len`.
    unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            std::ptr::null_mut(),
            &mut len,
        )
    };
    if len == 0 {
        return None;
    }
    let mut buffer = vec![0u16; len as usize];
    // SAFETY: `buffer` holds `len` wide chars, as the call asked for.
    let named = unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            buffer.as_mut_ptr(),
            &mut len,
        )
    };
    if named == 0 {
        return None;
    }
    // On success `len` counts the name's chars, without the NUL.
    Some(String::from_utf16_lossy(
        &buffer[..(len as usize).min(buffer.len())],
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_name_is_never_empty() {
        assert!(!super::name().is_empty());
    }
}
