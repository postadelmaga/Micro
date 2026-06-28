//! Two buses, two processes' worth of separation simulated by a TCP loopback: a value
//! published on bus A is delivered to a subscriber on bus B, with no module-side changes.

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use framelite_bridge::Bridge;
use framelite_bus::LocalBus;
use framelite_protocol::{Envelope, ModuleId};

#[test]
fn envelope_published_on_bus_a_arrives_on_bus_b() {
    // A loopback pair standing in for two processes.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).unwrap();
    let (server, _) = listener.accept().unwrap();
    // A read timeout so the ingress thread can be stopped promptly on a quiet link.
    server.set_read_timeout(Some(Duration::from_millis(50))).unwrap();

    let bus_a = Arc::new(LocalBus::new());
    let bus_b = Arc::new(LocalBus::new());

    // A → (TCP) → B, forwarding only the "data" channel.
    let egress = Bridge::egress(bus_a.clone(), [framelite_bus::Channel::new("data")], client);
    let ingress = Bridge::ingress(bus_b.clone(), server);

    // A subscriber on the *far* bus.
    let rx = bus_b.subscribe("data");

    // Publish on the near bus, as any module would.
    let env = Envelope::new(ModuleId::new("producer"), "data", serde_json::json!({ "v": 99 }));
    bus_a.publish(env).unwrap();

    // It crosses the boundary and is republished on B.
    let mut got = None;
    for _ in 0..100 {
        if let Ok(Some(env)) = rx.try_recv() {
            got = Some(env);
            break;
        }
        sleep(Duration::from_millis(10));
    }
    let got = got.expect("envelope did not cross the bridge");
    assert_eq!(got.from, ModuleId::new("producer"));
    assert_eq!(got.payload["v"], 99);

    egress.stop();
    ingress.stop();
}
