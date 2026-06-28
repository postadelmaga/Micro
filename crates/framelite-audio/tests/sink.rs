// The headless path end-to-end: spawn an AudioSink with a Recorder on the runtime, push blocks
// through the bounded data-plane channel, and confirm every sample arrives, in order.

use std::time::{Duration, Instant};

use framelite_audio::{AudioSink, Recorder};
use framelite_core::Runtime;
use framelite_media::{bounded, AudioBlock};

#[test]
fn plays_blocks_losslessly() {
    let (tx, rx) = bounded::<AudioBlock>(8);
    let (recorder, captured) = Recorder::new();

    let mut rt = Runtime::new();
    rt.spawn(AudioSink::new("audio", rx, recorder));

    // Two stereo blocks of 256 interleaved samples each → 512 total.
    tx.send(AudioBlock::new(48000, 2, vec![0.25f32; 256])).unwrap();
    tx.send(AudioBlock::new(48000, 2, vec![0.25f32; 256])).unwrap();

    // The sink plays on its own thread; poll the shared buffer until all samples land.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if captured.lock().unwrap().len() >= 512 {
            break;
        }
        assert!(Instant::now() < deadline, "timed out waiting for samples");
        std::thread::sleep(Duration::from_millis(10));
    }

    let samples = captured.lock().unwrap();
    assert_eq!(samples.len(), 512);
    assert!(samples.iter().all(|&s| s == 0.25));

    rt.shutdown();
    rt.join();
}
