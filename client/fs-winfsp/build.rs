// WinFsp ships as a system package; its import library is resolved via delay-loading.
// winfsp-rs provides the linker glue — call it on Windows only.
fn main() {
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
