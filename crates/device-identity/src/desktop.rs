use std::marker::PhantomData;
use tauri::Runtime;

pub struct DeviceIdentity<R: Runtime>(PhantomData<fn() -> R>);

pub fn init<R: Runtime>() -> DeviceIdentity<R> {
    DeviceIdentity(PhantomData)
}
