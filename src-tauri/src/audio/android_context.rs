use jni::{
    objects::{GlobalRef, JObject},
    sys::jboolean,
    JNIEnv,
};
use std::{
    ffi::c_void,
    sync::{Mutex, OnceLock},
};

// CPAL's AAudio backend reads ndk-context before opening a stream. This app is
// embedded in Tauri's JVM process (there is no Rust `main` binary); the plugin
// command initializes it before the first CPAL host access. Retain an
// Application Context (not a short-lived Activity) for the process lifetime.
static INIT_LOCK: Mutex<()> = Mutex::new(());
static APPLICATION_CONTEXT: OnceLock<GlobalRef> = OnceLock::new();

#[no_mangle]
pub extern "system" fn Java_com_championvibe_deviceidentity_DeviceIdentityPlugin_nativeInitializeAudioContext(
    env: JNIEnv<'_>,
    _this: JObject<'_>,
    context: JObject<'_>,
) -> jboolean {
    let Ok(_guard) = INIT_LOCK.lock() else {
        return 0;
    };
    if APPLICATION_CONTEXT.get().is_some() {
        return 1;
    }

    let Ok(global_context) = env.new_global_ref(&context) else {
        return 0;
    };
    let Ok(java_vm) = env.get_java_vm() else {
        return 0;
    };
    let vm_pointer = java_vm.get_java_vm_pointer() as *mut c_void;
    let context_pointer = global_context.as_obj().as_raw() as *mut c_void;

    // SAFETY: The JNI VM is live, the context is promoted to a process-lifetime
    // global reference, and INIT_LOCK guarantees this bridge calls initialize
    // at most once before any CPAL device/stream is created.
    unsafe {
        ndk_context::initialize_android_context(vm_pointer, context_pointer);
    }
    let _ = APPLICATION_CONTEXT.set(global_context);
    1
}
