//! Keeping the process's view of the host's MIDI devices current.

/// Holds what the process needs in order to notice devices coming and going.
///
/// Build one before looking for a device and keep it for as long as the process runs.
pub struct HostWatch {
    /// Registered with the host but connected to nothing, so there is somebody to notify.
    client: Option<midir::MidiOutput>,
}

impl core::fmt::Debug for HostWatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HostWatch")
            .field("registered", &self.client.is_some())
            .finish()
    }
}

impl HostWatch {
    /// Registers with the host.
    pub fn new() -> Self {
        Self {
            client: midir::MidiOutput::new("free-loop watch").ok(),
        }
    }

    /// Lets the host deliver any device changes it has queued.
    ///
    /// Call every pass of the control loop. On macOS the list of MIDI devices a process
    /// can see is only refreshed while that process runs its event loop, so without this
    /// a device attached after startup is never listed, however often it is asked for.
    /// Other platforms need nothing.
    #[cfg(target_os = "macos")]
    pub fn pump(&self) {
        use core_foundation::base::TCFType as _;
        use core_foundation::runloop::CFRunLoop;
        use core_foundation::string::CFString;

        let mode = CFString::from_static_string("kCFRunLoopDefaultMode");
        CFRunLoop::run_in_mode(
            mode.as_concrete_TypeRef(),
            core::time::Duration::ZERO,
            false,
        );
    }

    /// Lets the host deliver any device changes it has queued.
    #[cfg(not(target_os = "macos"))]
    pub fn pump(&self) {}
}

impl Default for HostWatch {
    fn default() -> Self {
        Self::new()
    }
}
