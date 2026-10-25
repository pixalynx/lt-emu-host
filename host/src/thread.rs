//! The emulator thread's scheduling.

/// Marks the calling thread as user-interactive work. On macOS that keeps it on the performance cores:
/// as a default-QoS thread it can land on an efficiency core, where a frame that normally takes 4 ms takes
/// several times longer and the sound runs dry. Elsewhere it does nothing yet.
pub fn interactive() {
    #[cfg(target_os = "macos")]
    // SAFETY: sets a scheduling hint on the calling thread only; no memory is touched.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}
