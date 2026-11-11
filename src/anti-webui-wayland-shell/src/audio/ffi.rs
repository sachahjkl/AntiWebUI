//! The parts of libpipewire this uses, found at run time. Nothing here comes from PipeWire's
//! headers other than what its interface is: the names and types of functions, the layout of the
//! structs passed, and the values of constants (from `pipewire-0.3` and `spa-0.2`).
//!
//! Most of PipeWire's calls are inline functions in C that call through the table of methods of an
//! object, which starts with a `spa_interface`; [`methods`] does the same.

use libloading::Library;
use std::ffi::{c_char, c_int, c_void};

pub type Object = c_void;

#[repr(C)]
pub struct SpaList {
    pub next: *mut SpaList,
    pub prev: *mut SpaList,
}

#[repr(C)]
pub struct SpaCallbacks {
    pub funcs: *const c_void,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct SpaInterface {
    pub kind: *const c_char,
    pub version: u32,
    pub cb: SpaCallbacks,
}

/// Where a listener is kept in the list of an object. It must stay where it is while listed.
#[repr(C)]
pub struct SpaHook {
    pub link: SpaList,
    pub cb: SpaCallbacks,
    pub removed: Option<unsafe extern "C" fn(*mut SpaHook)>,
    pub private: *mut c_void,
}

impl SpaHook {
    pub fn new() -> Box<Self> {
        Box::new(Self {
            link: SpaList {
                next: std::ptr::null_mut(),
                prev: std::ptr::null_mut(),
            },
            cb: SpaCallbacks {
                funcs: std::ptr::null(),
                data: std::ptr::null_mut(),
            },
            removed: None,
            private: std::ptr::null_mut(),
        })
    }

    /// Takes it out of the list it is in, as `spa_hook_remove` does.
    ///
    /// # Safety
    ///
    /// It must be in a list, and the objects around it in that list alive.
    pub unsafe fn remove(&mut self) {
        if self.link.next.is_null() {
            return;
        }
        // SAFETY: as the caller says.
        unsafe {
            (*self.link.prev).next = self.link.next;
            (*self.link.next).prev = self.link.prev;
        }
        self.link.next = std::ptr::null_mut();
        self.link.prev = std::ptr::null_mut();
        if let Some(removed) = self.removed {
            // SAFETY: what PipeWire asked to be told.
            unsafe { removed(self) };
        }
    }
}

#[repr(C)]
pub struct SpaDictItem {
    pub key: *const c_char,
    pub value: *const c_char,
}

#[repr(C)]
pub struct SpaDict {
    pub flags: u32,
    pub n_items: u32,
    pub items: *const SpaDictItem,
}

#[repr(C)]
pub struct SpaPod {
    pub size: u32,
    pub kind: u32,
}

#[repr(C)]
pub struct NodeInfo {
    pub id: u32,
    pub max_input_ports: u32,
    pub max_output_ports: u32,
    pub change_mask: u64,
    pub n_input_ports: u32,
    pub n_output_ports: u32,
    pub state: c_int,
    pub error: *const c_char,
    pub props: *const SpaDict,
    pub params: *const c_void,
    pub n_params: u32,
}

pub const NODE_CHANGE_MASK_PROPS: u64 = 1 << 3;

pub type AddListener<E> =
    unsafe extern "C" fn(*mut Object, *mut SpaHook, *const E, *mut c_void) -> c_int;

/// `pw_core_methods`, as far as `get_registry`.
#[repr(C)]
pub struct CoreMethods {
    pub version: u32,
    pub add_listener: AddListener<CoreEvents>,
    pub hello: *const c_void,
    pub sync: *const c_void,
    pub pong: *const c_void,
    pub error: *const c_void,
    pub get_registry: unsafe extern "C" fn(*mut Object, u32, usize) -> *mut Object,
}

/// `pw_core_events` of version 0. All of it, unlike the tables of methods: PipeWire calls every
/// event of the version given, so a shorter table would have it call what comes after.
#[repr(C)]
pub struct CoreEvents {
    pub version: u32,
    pub info: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    pub done: Option<unsafe extern "C" fn(*mut c_void, u32, c_int)>,
    pub ping: Option<unsafe extern "C" fn(*mut c_void, u32, c_int)>,
    pub error: Option<unsafe extern "C" fn(*mut c_void, u32, c_int, c_int, *const c_char)>,
    pub remove_id: Option<unsafe extern "C" fn(*mut c_void, u32)>,
    pub bound_id: Option<unsafe extern "C" fn(*mut c_void, u32, u32)>,
    pub add_mem: Option<unsafe extern "C" fn(*mut c_void, u32, u32, c_int, u32)>,
    pub remove_mem: Option<unsafe extern "C" fn(*mut c_void, u32)>,
}

/// `pw_registry_methods`, as far as `bind`.
#[repr(C)]
pub struct RegistryMethods {
    pub version: u32,
    pub add_listener: AddListener<RegistryEvents>,
    pub bind: unsafe extern "C" fn(*mut Object, u32, *const c_char, u32, usize) -> *mut Object,
}

#[repr(C)]
pub struct RegistryEvents {
    pub version: u32,
    pub global:
        Option<unsafe extern "C" fn(*mut c_void, u32, u32, *const c_char, u32, *const SpaDict)>,
    pub global_remove: Option<unsafe extern "C" fn(*mut c_void, u32)>,
}

/// `pw_node_methods`, as far as `set_param`.
#[repr(C)]
pub struct NodeMethods {
    pub version: u32,
    pub add_listener: AddListener<NodeEvents>,
    pub subscribe_params: unsafe extern "C" fn(*mut Object, *mut u32, u32) -> c_int,
    pub enum_params: *const c_void,
    pub set_param: unsafe extern "C" fn(*mut Object, u32, u32, *const SpaPod) -> c_int,
}

#[repr(C)]
pub struct NodeEvents {
    pub version: u32,
    pub info: Option<unsafe extern "C" fn(*mut c_void, *const NodeInfo)>,
    pub param: Option<unsafe extern "C" fn(*mut c_void, c_int, u32, u32, u32, *const SpaPod)>,
}

/// `pw_device_info`: a card.
#[repr(C)]
pub struct DeviceInfo {
    pub id: u32,
    pub change_mask: u64,
    pub props: *const SpaDict,
    pub params: *const ParamInfo,
    pub n_params: u32,
}

/// `spa_param_info`: a param of an object, and flags that change when it does (the server
/// flips `SPA_PARAM_INFO_SERIAL` each time).
#[repr(C)]
pub struct ParamInfo {
    pub id: u32,
    pub flags: u32,
    pub user: u32,
    pub seq: i32,
    pub padding: [u32; 4],
}

/// `SPA_PARAM_INFO_READ`.
pub const PARAM_INFO_READ: u32 = 1 << 1;

pub const DEVICE_CHANGE_MASK_PROPS: u64 = 1 << 0;
pub const DEVICE_CHANGE_MASK_PARAMS: u64 = 1 << 1;

/// `pw_device_methods`, as far as `set_param`.
#[repr(C)]
pub struct DeviceMethods {
    pub version: u32,
    pub add_listener: AddListener<DeviceEvents>,
    pub subscribe_params: unsafe extern "C" fn(*mut Object, *mut u32, u32) -> c_int,
    pub enum_params:
        unsafe extern "C" fn(*mut Object, c_int, u32, u32, u32, *const SpaPod) -> c_int,
    pub set_param: unsafe extern "C" fn(*mut Object, u32, u32, *const SpaPod) -> c_int,
}

#[repr(C)]
pub struct DeviceEvents {
    pub version: u32,
    pub info: Option<unsafe extern "C" fn(*mut c_void, *const DeviceInfo)>,
    pub param: Option<unsafe extern "C" fn(*mut c_void, c_int, u32, u32, u32, *const SpaPod)>,
}

/// `pw_metadata_methods`, as far as `set_property`.
#[repr(C)]
pub struct MetadataMethods {
    pub version: u32,
    pub add_listener: AddListener<MetadataEvents>,
    pub set_property: unsafe extern "C" fn(
        *mut Object,
        u32,
        *const c_char,
        *const c_char,
        *const c_char,
    ) -> c_int,
}

#[repr(C)]
pub struct MetadataEvents {
    pub version: u32,
    pub property: Option<
        unsafe extern "C" fn(
            *mut c_void,
            u32,
            *const c_char,
            *const c_char,
            *const c_char,
        ) -> c_int,
    >,
}

pub const VERSION_REGISTRY: u32 = 3;
pub const VERSION_NODE: u32 = 3;
pub const VERSION_METADATA: u32 = 3;
pub const VERSION_DEVICE: u32 = 3;
pub const INTERFACE_NODE: &[u8] = b"PipeWire:Interface:Node\0";
pub const INTERFACE_DEVICE: &[u8] = b"PipeWire:Interface:Device\0";
pub const INTERFACE_METADATA: &[u8] = b"PipeWire:Interface:Metadata\0";
pub const INTERFACE_LINK: &[u8] = b"PipeWire:Interface:Link\0";

/// The table of methods of an object and what to call them with.
///
/// # Safety
///
/// `object` must be a live PipeWire proxy whose methods are an `M`.
pub unsafe fn methods<'a, M>(object: *mut Object) -> (&'a M, *mut Object) {
    // SAFETY: proxies start with their `spa_interface`, as the caller says this is one.
    unsafe {
        let iface = &*(object as *const SpaInterface);
        (&*(iface.cb.funcs as *const M), iface.cb.data)
    }
}

/// The functions of libpipewire this calls.
pub struct Lib {
    _lib: Library,
    pub init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    pub thread_loop_new: unsafe extern "C" fn(*const c_char, *const SpaDict) -> *mut Object,
    pub thread_loop_get_loop: unsafe extern "C" fn(*mut Object) -> *mut Object,
    pub thread_loop_start: unsafe extern "C" fn(*mut Object) -> c_int,
    pub thread_loop_lock: unsafe extern "C" fn(*mut Object),
    pub thread_loop_unlock: unsafe extern "C" fn(*mut Object),
    pub context_new: unsafe extern "C" fn(*mut Object, *mut Object, usize) -> *mut Object,
    pub context_connect: unsafe extern "C" fn(*mut Object, *mut Object, usize) -> *mut Object,
    pub proxy_destroy: unsafe extern "C" fn(*mut Object),
}

impl Lib {
    pub fn load() -> Result<Self, String> {
        // SAFETY: loading the system's libpipewire, whose initializers are safe to run.
        let lib = unsafe { Library::new("libpipewire-0.3.so.0") }.map_err(|e| e.to_string())?;
        macro_rules! sym {
            ($name:literal) => {
                // SAFETY: the symbol has the type of the field it goes into (see the headers).
                *unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                    .map_err(|e| format!("{}: {e}", $name))?
            };
        }
        Ok(Self {
            init: sym!("pw_init"),
            thread_loop_new: sym!("pw_thread_loop_new"),
            thread_loop_get_loop: sym!("pw_thread_loop_get_loop"),
            thread_loop_start: sym!("pw_thread_loop_start"),
            thread_loop_lock: sym!("pw_thread_loop_lock"),
            thread_loop_unlock: sym!("pw_thread_loop_unlock"),
            context_new: sym!("pw_context_new"),
            context_connect: sym!("pw_context_connect"),
            proxy_destroy: sym!("pw_proxy_destroy"),
            _lib: lib,
        })
    }
}

/// The items of a dict, as strings.
///
/// # Safety
///
/// `dict` must be null or a valid `spa_dict`.
pub unsafe fn dict(dict: *const SpaDict) -> Vec<(String, String)> {
    if dict.is_null() {
        return Vec::new();
    }
    // SAFETY: as the caller says.
    let d = unsafe { &*dict };
    if d.items.is_null() {
        return Vec::new();
    }
    // SAFETY: `n_items` items are there.
    let items = unsafe { std::slice::from_raw_parts(d.items, d.n_items as usize) };
    items
        .iter()
        .filter_map(|i| {
            // SAFETY: the strings of a dict are null-terminated, or null.
            Some((
                unsafe { string(i.key)? },
                unsafe { string(i.value) }.unwrap_or_default(),
            ))
        })
        .collect()
}

/// A C string, if not null.
///
/// # Safety
///
/// `s` must be null or null-terminated.
pub unsafe fn string(s: *const c_char) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: as the caller says.
    Some(
        unsafe { std::ffi::CStr::from_ptr(s) }
            .to_string_lossy()
            .into_owned(),
    )
}
