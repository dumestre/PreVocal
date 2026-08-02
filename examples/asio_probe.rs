use asio_sys::Asio;

fn main() {
    let asio = Asio::new();
    let names = asio.driver_names();
    println!("Registered ASIO drivers: {names:?}");
    for name in names {
        let result = {
            let driver = asio.load_driver(&name);
            match driver {
                Ok(d) => {
                    let ch = d.channels();
                    let bs = d.buffersize_range();
                    let sr = d.sample_rate();
                    format!("OK channels={ch:?} buffers={bs:?} rate={sr:?}")
                }
                Err(e) => format!("FAILED {e:?}"),
            }
        };
        println!("{name}: {result}");
    }
}
