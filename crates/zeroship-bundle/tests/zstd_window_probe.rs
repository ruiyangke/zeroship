//! Whether `zstd::Decoder` allocates a frame's DECLARED window eagerly, and
//! therefore whether `.window_log_max(...)` is a fix for deploy-time memory
//! amplification.
//!
//! It is not, and this test exists so the next person does not add it.
//!
//! MEASURED: eight decoders over a frame declaring a 128 MiB window construct
//! AND read with no measurable RSS growth. If the window were allocated at
//! header parse the delta would be about a gigabyte. zstd sizes the window to
//! what the content actually needs.
//!
//! The encoder cannot be used to build the fixture: given small content it
//! shrinks the window descriptor to fit, so a frame built with
//! `CParameter::WindowLog(27)` and a five-byte payload still declares a tiny
//! window - the first version of this test measured exactly that and concluded
//! nothing. The frame is therefore hand-built, with Single_Segment clear and no
//! Frame_Content_Size, leaving the decoder nothing to clamp against.

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Frame declaring a 128 MiB window (Window_Descriptor exponent 17 -> 2^27),
/// carrying a single raw block of five bytes.
fn frame_declaring_128mib_window() -> Vec<u8> {
    let mut f = vec![0x28, 0xB5, 0x2F, 0xFD]; // magic
    f.push(0x00); // FHD: no Frame_Content_Size, Single_Segment clear
    f.push(17u8 << 3); // Window_Descriptor: exponent 17, mantissa 0
    let payload = b"hello";
    let bh: u32 = ((payload.len() as u32) << 3) | 1; // raw block, last
    f.extend_from_slice(&bh.to_le_bytes()[..3]);
    f.extend_from_slice(payload);
    f
}

const N: usize = 8;

/// The fixture really does declare a window larger than 8 MiB.
///
/// Without this, the no-growth result below would be equally consistent with a
/// frame that declares a small window, which is the failure the first draft of
/// this test actually had.
#[test]
fn the_fixture_declares_a_window_larger_than_the_cap() {
    let frame = frame_declaring_128mib_window();
    let mut d = zstd::Decoder::new(std::io::Cursor::new(frame)).unwrap();
    d.window_log_max(23).unwrap(); // 8 MiB
    let mut sink = [0u8; 1];
    assert!(
        std::io::Read::read(&mut d, &mut sink).is_err(),
        "a decoder capped at 8 MiB accepted this frame, so the frame does not \
         declare a larger window and the eager-allocation test below proves nothing"
    );
}

#[test]
fn zstd_does_not_allocate_the_declared_window_eagerly() {
    let frame = frame_declaring_128mib_window();
    let mut sink = [0u8; 1];

    let before = rss_kb();
    let mut decoders: Vec<_> = (0..N)
        .map(|_| zstd::Decoder::new(std::io::Cursor::new(frame.clone())).unwrap())
        .collect();
    for d in &mut decoders {
        let _ = std::io::Read::read(d, &mut sink);
    }
    let grew_mb = (rss_kb().saturating_sub(before)) / 1024;

    // Eager allocation would be N * 128 MiB = 1 GiB. The threshold is loose on
    // purpose: this is asserting an order of magnitude, not an allocator.
    assert!(
        grew_mb < 64,
        "constructing and reading {N} decoders over 128 MiB-window frames grew \
         RSS by {grew_mb} MB, which suggests the window IS allocated eagerly - \
         if so, capping it is worth revisiting for deploy ingest"
    );
}
