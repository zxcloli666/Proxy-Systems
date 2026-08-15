use std::fs;

const SYSCTL_PATH: &str = "/proc/sys/net/ipv6/ip_nonlocal_bind";

pub enum State {
    AlreadyOn,
    TurnedOn,
    Failed(String),
}

pub fn ensure() -> State {
    match fs::read_to_string(SYSCTL_PATH) {
        Ok(current) if current.trim() == "1" => return State::AlreadyOn,
        Ok(_) => {}
        Err(e) => return State::Failed(format!("{SYSCTL_PATH}: {e}")),
    }

    match fs::write(SYSCTL_PATH, b"1") {
        Ok(()) => State::TurnedOn,
        Err(e) => State::Failed(format!("{SYSCTL_PATH}: {e}")),
    }
}

pub fn report(state: State) {
    match state {
        State::AlreadyOn => {
            tracing::info!("net.ipv6.ip_nonlocal_bind is on: source rotation can bind the prefix")
        }
        State::TurnedOn => {
            tracing::info!("net.ipv6.ip_nonlocal_bind switched on for source rotation")
        }
        State::Failed(reason) => tracing::error!(
            "IPV6_SUBNET is set but net.ipv6.ip_nonlocal_bind could not be enabled ({reason}). \
             Binding an address that is routed but not assigned will fail with EADDRNOTAVAIL. \
             Start the container with --sysctl net.ipv6.ip_nonlocal_bind=1, or set it on the host."
        ),
    }
}
