use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    for host_id in cpal::available_hosts() {
        println!("Host: {}", host_id.name());
        let Ok(host) = cpal::host_from_id(host_id) else {
            println!("  (unavailable)");
            continue;
        };
        let Ok(devices) = host.devices() else {
            println!("  (devices() failed)");
            continue;
        };
        for device in devices {
            let name = device.to_string();
            println!("  device: {name} in={} out={}", device.supports_input(), device.supports_output());
        }
    }
}
