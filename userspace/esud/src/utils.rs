use std::{
    ffi::{CStr, CString, c_char, c_void},
    fs::OpenOptions,
    io::Write,
    path::Path,
};

type PropertyReadCallback = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, u32);

unsafe extern "C" {
    fn __system_property_find(name: *const c_char) -> *const c_void;
    fn __system_property_read_callback(
        property_info: *const c_void,
        callback: PropertyReadCallback,
        cookie: *mut c_void,
    );
}

#[macro_export]
macro_rules! debug_select {
    ($debug:expr, $release:expr) => {{
        #[cfg(debug_assertions)]
        {
            $debug
        }
        #[cfg(not(debug_assertions))]
        {
            $release
        }
    }};
}

unsafe extern "C" fn property_read_callback(
    cookie: *mut c_void,
    _name: *const c_char,
    value: *const c_char,
    _serial: u32,
) {
    if cookie.is_null() || value.is_null() {
        return;
    }

    let result = unsafe { &mut *cookie.cast::<Option<String>>() };
    let value = unsafe { CStr::from_ptr(value) };
    *result = Some(value.to_string_lossy().into_owned());
}

pub fn getprop(name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    let property_info = unsafe { __system_property_find(name.as_ptr()) };
    if property_info.is_null() {
        return None;
    }

    let mut value = None;
    unsafe {
        __system_property_read_callback(
            property_info,
            property_read_callback,
            std::ptr::addr_of_mut!(value).cast(),
        );
    }
    value
}

fn switch_cgroup(grp: &str, pid: u32) {
    let path = Path::new(grp).join("cgroup.procs");
    if !path.exists() {
        return;
    }

    let fp = OpenOptions::new().append(true).open(path);
    if let std::result::Result::Ok(mut fp) = fp {
        let _ = write!(fp, "{pid}");
    }
}

pub fn switch_cgroups() {
    let pid = std::process::id();
    switch_cgroup("/acct", pid);
    switch_cgroup("/dev/cg2_bpf", pid);
    switch_cgroup("/sys/fs/cgroup", pid);

    if getprop("ro.config.per_app_memcg")
        .as_ref()
        .is_none_or(|prop| prop != "false")
    {
        switch_cgroup("/dev/memcg/apps", pid);
    }
}
