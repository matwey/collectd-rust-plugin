use api::{empty_to_none, get_default_interval, log_err, CdTime, ConfigItem, LogLevel, ValueList};
use bindings::{
    cdtime_t, data_set_t, oconfig_item_t, plugin_register_complex_read, plugin_register_flush,
    plugin_register_log, plugin_register_write, user_data_t, value_list_t,
};
use errors::{CollectdUtf8Error, FfiError};
use plugins::{Plugin, PluginManager, PluginManagerCapabilities, PluginRegistration};
use std::ffi::{CStr, CString};
use std::ops::Deref;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::catch_unwind;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

extern "C" fn plugin_read(dt: *mut user_data_t) -> c_int {
    let plugin = unsafe { &mut *((*dt).data as *mut Box<Plugin>) };
    let res = catch_unwind(|| plugin.read_values())
        .map_err(|_| FfiError::Panic)
        .and_then(|x| x.map_err(FfiError::Plugin));

    if let Err(ref e) = res {
        log_err("read", e);
    }

    res.map(|_| 0).unwrap_or(-1)
}

extern "C" fn plugin_log(severity: c_int, message: *const c_char, dt: *mut user_data_t) {
    let plugin = unsafe { &mut *((*dt).data as *mut Box<Plugin>) };

    // Guard against potential null messages even if they are not supposed to happen.
    if message.is_null() {
        return;
    }

    // Here we allow the potential allocation of a string that contains replacement
    // characters as it wouldn't be right if collectd-plugin stopped the logging of an
    // important message when a small portion of the message may be illegible.
    let msg = unsafe { CStr::from_ptr(message).to_string_lossy() };
    let res = LogLevel::try_from(severity as u32)
        .ok_or_else(|| FfiError::UnknownSeverity(severity))
        .and_then(|lvl| {
            catch_unwind(|| plugin.log(lvl, Deref::deref(&msg)))
                .map_err(|_| FfiError::Panic)
                .and_then(|x| x.map_err(FfiError::Plugin))
        });

    if let Err(ref e) = res {
        log_err("logging", e);
    }
}

extern "C" fn plugin_write(
    ds: *const data_set_t,
    vl: *const value_list_t,
    dt: *mut user_data_t,
) -> c_int {
    let plugin = unsafe { &mut *((*dt).data as *mut Box<Plugin>) };
    match unsafe { ValueList::from(&*ds, &*vl) } {
        Ok(list) => {
            let res = catch_unwind(|| plugin.write_values(list))
                .map_err(|_| FfiError::Panic)
                .and_then(|x| x.map_err(FfiError::Plugin));

            if let Err(ref e) = res {
                log_err("writing", e);
            }

            res.map(|_| 0).unwrap_or(-1)
        }
        Err(e) => {
            log_err(
                "unable to decode collectd data",
                &FfiError::Collectd(Box::new(e)),
            );
            -1
        }
    }
}

extern "C" fn plugin_flush(
    timeout: cdtime_t,
    identifier: *const c_char,
    dt: *mut user_data_t,
) -> c_int {
    let plugin = unsafe { &mut *((*dt).data as *mut Box<::Plugin>) };

    let dur = if timeout == 0 {
        None
    } else {
        Some(CdTime::from(timeout).into())
    };

    let ident = if identifier.is_null() {
        Ok(None)
    } else {
        unsafe { CStr::from_ptr(identifier) }
            .to_str()
            .map(empty_to_none)
            .map_err(|e| CollectdUtf8Error("flush identifier", e))
            .map_err(|e| FfiError::Collectd(Box::new(e)))
    };

    let res = ident.and_then(|id| {
        catch_unwind(|| plugin.flush(dur, id))
            .map_err(|_| FfiError::Panic)
            .and_then(|x| x.map_err(FfiError::Plugin))
    });

    if let Err(ref e) = res {
        log_err("flush", e);
    }

    res.map(|_| 0).unwrap_or(-1)
}

unsafe extern "C" fn plugin_free_user_data(raw: *mut c_void) {
    let ptr = raw as *mut Box<Plugin>;
    drop(Box::from_raw(ptr));
}

fn plugin_registration(name: &str, plugin: Box<Plugin>) {
    let pl: Box<Box<Plugin>> = Box::new(plugin);

    // Grab all the properties we need until `into_raw` away
    let should_read = pl.capabilities().has_read();
    let should_log = pl.capabilities().has_log();
    let should_write = pl.capabilities().has_write();
    let should_flush = pl.capabilities().has_flush();

    let s = CString::new(name).expect("Plugin name to not contain nulls");

    // Plugin registration differs only a tiny bit between collectd-57 and older
    // versions. The one difference is that user_data_t went from mutable to not
    // mutable. The code duplication is annoying, but it's better to have it
    // encapsulated in a single crate instead of many others.
    #[cfg_attr(feature = "cargo-clippy", allow(unnecessary_mut_passed))]
    unsafe {
        let plugin_ptr = Box::into_raw(pl) as *mut c_void;

        // The user data that is passed to read, writes, logs, etc. It is not passed to
        // config or init. Since user_data_t implements copy, we don't need to forget about
        // it. See clippy suggestion (forget_copy)
        let mut data = user_data_t {
            data: plugin_ptr,
            free_func: Some(plugin_free_user_data),
        };

        // If a plugin registers more than one callback, we make sure to deregister the
        // free function to avoid data being freed twice:
        // https://collectd.org/wiki/index.php/User_data_t
        let mut no_free_data = user_data_t {
            data: plugin_ptr,
            free_func: None,
        };

        if should_read {
            plugin_register_complex_read(
                ptr::null(),
                s.as_ptr(),
                Some(plugin_read),
                get_default_interval(),
                &mut data,
            );
        }

        if should_write {
            let d = if !should_read {
                &mut data
            } else {
                &mut no_free_data
            };

            plugin_register_write(s.as_ptr(), Some(plugin_write), d);
        }

        if should_log {
            let d = if !should_read && !should_write {
                &mut data
            } else {
                &mut no_free_data
            };

            plugin_register_log(s.as_ptr(), Some(plugin_log), d);
        }

        if should_flush {
            let d = if !should_read && !should_write && !should_log {
                &mut data
            } else {
                &mut no_free_data
            };

            plugin_register_flush(s.as_ptr(), Some(plugin_flush), d);
        }
    }
}

fn register_all_plugins<T: PluginManager>(config: Option<&[ConfigItem]>) -> c_int {
    let res = catch_unwind(|| T::plugins(config))
        .map_err(|_| FfiError::Panic)
        .and_then(|reged| reged.map_err(FfiError::Plugin))
        .and_then(|registration| {
            match registration {
                PluginRegistration::Single(pl) => {
                    plugin_registration(T::name(), pl);
                }
                PluginRegistration::Multiple(v) => {
                    for (id, pl) in v {
                        let name = format!("{}/{}", T::name(), id);

                        plugin_registration(name.as_str(), pl);
                    }
                }
            }

            // TODO: remove this ok
            Ok(())
        });

    if let Err(ref e) = res {
        log_err("collectd config", e);
    }
    res.map(|_| 0).unwrap_or(-1)
}

pub fn plugin_init<T: PluginManager>(config_seen: &AtomicBool) -> c_int {
    let mut result = if !config_seen.swap(true, Ordering::Relaxed) {
        register_all_plugins::<T>(None)
    } else {
        0
    };

    let capabilities = T::capabilities();
    if capabilities.intersects(PluginManagerCapabilities::INIT) {
        let res = catch_unwind(T::initialize)
            .map_err(|_e| FfiError::Panic)
            .and_then(|init| init.map_err(FfiError::Plugin));

        if let Err(ref e) = res {
            result = -1;
            log_err("init", e);
        }
    }

    result
}

pub unsafe fn plugin_complex_config<T: PluginManager>(
    config_seen: &AtomicBool,
    config: *mut oconfig_item_t,
) -> c_int {
    // If we've already seen the config, let's error out as one shouldn't use multiple
    // sections of configuration (group them under nodes like write_graphite)
    if config_seen.swap(true, Ordering::Relaxed) {
        log_err("config", &FfiError::MultipleConfig);
        return -1;
    }

    match ConfigItem::from(&*config) {
        Ok(config) => register_all_plugins::<T>(Some(&config.children)),
        Err(e) => {
            log_err(
                "collectd config conversion",
                &FfiError::Collectd(Box::new(e)),
            );
            -1
        }
    }
}
