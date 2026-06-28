//! End-to-end of the video path with no GPU: feed frames through a `latest` mailbox into a
//! `VideoSink` running on the real `Runtime`, and read them back via a headless `BufferSink`.

use std::time::{Duration, Instant};

use framelite_core::Runtime;
use framelite_media::{latest, Frame, PixelFormat};
use framelite_video::{BufferSink, VideoSink};

#[test]
fn video_sink_presents_latest_frame() {
    let (tx, rx) = latest::<Frame>();
    let (sink, state) = BufferSink::new();

    let mut rt = Runtime::new();
    rt.spawn(VideoSink::new("video", rx, sink));

    // 4x4 RGBA = 4*4*4 = 64 bytes. Send a couple; latest-wins may coalesce them.
    let pixels = vec![0u8; 4 * 4 * 4];
    tx.send(Frame::new(4, 4, PixelFormat::Rgba8, pixels.clone()).unwrap())
        .unwrap();
    tx.send(Frame::new(4, 4, PixelFormat::Rgba8, pixels).unwrap())
        .unwrap();

    // Poll the shared state until at least one frame was presented (up to ~1s).
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if state.lock().unwrap().count >= 1 {
            break;
        }
        assert!(Instant::now() < deadline, "no frame presented within 1s");
        std::thread::sleep(Duration::from_millis(10));
    }

    let guard = state.lock().unwrap();
    let last = guard.last.as_ref().expect("a frame was recorded");
    assert_eq!(last.width, 4);
    assert_eq!(last.height, 4);
    drop(guard);

    rt.shutdown();
    rt.join();
}
