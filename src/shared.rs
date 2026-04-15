/// Events sent from Discovery (process scanner) to Gateway.
#[allow(dead_code)]
pub enum DiscoveryEvent {
    /// A new game process was detected.
    GameDetected {
        app_id: String,
        app_name: String,
        pid: u32,
    },
    /// A previously tracked game process has exited.
    GameExited { app_id: String },
}
