//! Gathers a download's backend frames into the chunks `readChunk` returns.
//!
//! A backend yields whatever its transport delivers: the S3 body arrives in
//! frames the size of one socket read, far smaller than the cost of a V8
//! promise round trip warrants. A [`Download`] copies frames into one buffer
//! until it holds [`DOWNLOAD_CHUNK_BYTES`](crate::limits::DOWNLOAD_CHUNK_BYTES)
//! or the object ends, and keeps the part of a frame that crossed the
//! boundary for the next chunk. A backend that trickles frames gets a partial
//! chunk instead: a gather ends at the first frame to arrive once
//! [`DOWNLOAD_GATHER_BUDGET`](crate::limits::DOWNLOAD_GATHER_BUDGET) has
//! passed since its first byte.
//!
//! Frames are pulled only while a `readChunk` is waiting, so a consumer that
//! stops reading stops the backend. Each frame still arrives under the
//! backend's own stall bound, and a failure ends the download at once.
//!
//! The budget is checked as frames arrive rather than raced against a timer.
//! A timer would have to abandon the frame being awaited, and a backend's
//! stall clock for that frame keeps running whether or not anyone polls it,
//! so a consumer that paused longer than the stall bound would come back to
//! a spurious stall. Ending at the next frame instead costs at most one more
//! frame wait, and no pull outlives the `readChunk` that started it.

use std::time::{Duration, Instant};

use bytes::Bytes;
use zeroship_storage::backend::BoxByteStream;
use zeroship_storage::StorageError;

/// One download's backend source and the unread part of its last frame.
pub struct Download {
    source: BoxByteStream,
    carry: Bytes,
    /// Bytes the backend advertised and no chunk has delivered yet. Used only
    /// to size a chunk's buffer, never trusted beyond the chunk bound.
    unread_hint: u64,
}

/// The result of gathering one chunk.
#[derive(Debug)]
pub enum Gathered {
    /// Bytes of the object, never none; more of it may follow.
    Chunk(Vec<u8>),
    /// The object ended after these bytes, which may be none.
    Last(Vec<u8>),
    /// The backend failed. Bytes gathered for this chunk are discarded.
    Failed(StorageError),
    /// The handle was closed before the gather ended, whatever it gathered.
    Closed,
}

impl Download {
    pub fn new(source: BoxByteStream, size_hint: u64) -> Self {
        Self {
            source,
            carry: Bytes::new(),
            unread_hint: size_hint,
        }
    }

    /// Gather the next chunk of at most `limit` bytes, ending early at the
    /// first frame to arrive once `budget` has passed since the chunk's first
    /// byte. A chunk is never empty unless the object ended.
    ///
    /// `is_open` is checked as each frame arrives, so a handle closed
    /// mid-gather stops pulling at the next frame, and again as the gather
    /// ends, so every outcome of a closed handle is [`Gathered::Closed`]:
    /// a chunk served from the carry and an end or failure that pulled no
    /// frame included.
    #[expect(
        clippy::future_not_send,
        reason = "a download's backend source belongs to its compio thread"
    )]
    pub async fn gather(
        &mut self,
        limit: usize,
        budget: Duration,
        is_open: impl Fn() -> bool,
    ) -> Gathered {
        let gathered = self.fill(limit, budget, &is_open).await;
        if is_open() { gathered } else { Gathered::Closed }
    }

    #[expect(
        clippy::future_not_send,
        reason = "a download's backend source belongs to its compio thread"
    )]
    async fn fill(
        &mut self,
        limit: usize,
        budget: Duration,
        is_open: &impl Fn() -> bool,
    ) -> Gathered {
        let hint = usize::try_from(self.unread_hint).unwrap_or(limit);
        let mut chunk = Vec::with_capacity(limit.min(hint.max(self.carry.len())));
        if !self.carry.is_empty() {
            let take = self.carry.len().min(limit);
            chunk.extend_from_slice(&self.carry.split_to(take));
        }
        let mut first_byte = (!chunk.is_empty()).then(Instant::now);
        while chunk.len() < limit && first_byte.is_none_or(|at| at.elapsed() < budget) {
            match self.source.next_chunk().await {
                Some(Ok(frame)) => {
                    if !is_open() {
                        return Gathered::Closed;
                    }
                    let room = limit - chunk.len();
                    if frame.len() > room {
                        chunk.extend_from_slice(&frame[..room]);
                        self.carry = frame.slice(room..);
                    } else {
                        chunk.extend_from_slice(&frame);
                    }
                    if first_byte.is_none() && !chunk.is_empty() {
                        first_byte = Some(Instant::now());
                    }
                }
                Some(Err(error)) => return Gathered::Failed(error),
                None => return Gathered::Last(chunk),
            }
        }
        self.unread_hint = self.unread_hint.saturating_sub(chunk.len() as u64);
        Gathered::Chunk(chunk)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use zeroship_storage::backend::{ChunkResult, ChunkSource};

    use super::*;

    /// A budget no test with always-ready frames can reach.
    const AMPLE: Duration = Duration::from_mins(10);

    /// Yields scripted frames, then the scripted end, counting every pull.
    struct Script {
        frames: VecDeque<ChunkResult>,
        pulls: Rc<Cell<usize>>,
    }

    #[async_trait::async_trait(?Send)]
    impl ChunkSource for Script {
        async fn next_chunk(&mut self) -> Option<ChunkResult> {
            self.pulls.set(self.pulls.get() + 1);
            self.frames.pop_front()
        }
    }

    /// A download whose source yields `frames`, then `failure` if one is given,
    /// then the end.
    fn scripted(
        frames: &[&str],
        failure: Option<StorageError>,
        size_hint: u64,
    ) -> (Download, Rc<Cell<usize>>) {
        let mut script: VecDeque<ChunkResult> = frames
            .iter()
            .map(|frame| Ok(Bytes::copy_from_slice(frame.as_bytes())))
            .collect();
        script.extend(failure.map(Err));
        let pulls = Rc::new(Cell::new(0));
        let source = Script {
            frames: script,
            pulls: Rc::clone(&pulls),
        };
        (Download::new(Box::new(source), size_hint), pulls)
    }

    /// Serves `count` one-byte frames, each byte its frame number, waiting
    /// `delay(n)` before frame `n` as a slow transport would.
    struct Paced {
        next: u8,
        count: u8,
        delay: fn(u8) -> Duration,
    }

    #[async_trait::async_trait(?Send)]
    impl ChunkSource for Paced {
        async fn next_chunk(&mut self) -> Option<ChunkResult> {
            if self.next == self.count {
                return None;
            }
            compio::time::sleep((self.delay)(self.next)).await;
            self.next += 1;
            Some(Ok(Bytes::copy_from_slice(&[self.next - 1])))
        }
    }

    fn paced(count: u8, delay: fn(u8) -> Duration) -> Download {
        let source = Paced { next: 0, count, delay };
        Download::new(Box::new(source), u64::from(count))
    }

    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        compio::runtime::Runtime::new().unwrap().block_on(future)
    }

    fn chunk(gathered: Gathered) -> Vec<u8> {
        match gathered {
            Gathered::Chunk(bytes) => bytes,
            other => panic!("expected a chunk, got {other:?}"),
        }
    }

    fn last(gathered: Gathered) -> Vec<u8> {
        match gathered {
            Gathered::Last(bytes) => bytes,
            other => panic!("expected the object's end, got {other:?}"),
        }
    }

    fn closed(gathered: &Gathered) {
        assert!(matches!(gathered, Gathered::Closed), "expected the close, got {gathered:?}");
    }

    #[test]
    fn frames_are_gathered_into_chunks_of_exactly_the_limit() {
        let (mut download, _) = scripted(&["abc", "def", "ghi", "jk"], None, 11);
        run(async {
            assert_eq!(chunk(download.gather(4, AMPLE, || true).await), b"abcd");
            assert_eq!(chunk(download.gather(4, AMPLE, || true).await), b"efgh");
            assert_eq!(last(download.gather(4, AMPLE, || true).await), b"ijk");
        });
    }

    #[test]
    fn a_frame_larger_than_several_chunks_is_served_from_the_carry_without_pulling_again() {
        let (mut download, pulls) = scripted(&["0123456789"], None, 10);
        run(async {
            assert_eq!(chunk(download.gather(4, AMPLE, || true).await), b"0123");
            assert_eq!(chunk(download.gather(4, AMPLE, || true).await), b"4567");
            assert_eq!(pulls.get(), 1, "the carry covers the second chunk");
            assert_eq!(last(download.gather(4, AMPLE, || true).await), b"89");
        });
    }

    #[test]
    fn an_object_smaller_than_one_chunk_ends_with_its_bytes() {
        let (mut download, _) = scripted(&["tiny"], None, 4);
        run(async {
            assert_eq!(last(download.gather(1024, AMPLE, || true).await), b"tiny");
        });
    }

    #[test]
    fn an_empty_object_ends_with_no_bytes() {
        let (mut download, _) = scripted(&[], None, 0);
        run(async {
            assert!(last(download.gather(1024, AMPLE, || true).await).is_empty());
        });
    }

    #[test]
    fn an_object_that_fills_its_last_chunk_exactly_ends_on_the_following_gather() {
        let (mut download, _) = scripted(&["abcd"], None, 4);
        run(async {
            assert_eq!(chunk(download.gather(4, AMPLE, || true).await), b"abcd");
            assert!(last(download.gather(4, AMPLE, || true).await).is_empty());
        });
    }

    #[test]
    fn a_backend_failure_ends_the_chunk_with_the_failure() {
        let failure = StorageError::Stream("storage: s3 get body: stalled".into());
        let (mut download, _) = scripted(&["ab"], Some(failure), 100);
        run(async {
            match download.gather(64, AMPLE, || true).await {
                Gathered::Failed(error) => assert!(error.to_string().contains("stalled"), "{error}"),
                other => panic!("expected the backend failure, got {other:?}"),
            }
        });
    }

    #[test]
    fn a_wrong_size_hint_neither_truncates_nor_lets_a_chunk_pass_the_limit() {
        let (mut download, _) = scripted(&["abcdef", "ghij"], None, 2);
        run(async {
            assert_eq!(chunk(download.gather(8, AMPLE, || true).await), b"abcdefgh");
            assert_eq!(last(download.gather(8, AMPLE, || true).await), b"ij");
        });
    }

    #[test]
    fn a_trickling_backend_gets_a_partial_chunk_once_the_budget_passes() {
        // Filling the whole limit at this pace takes far longer than the
        // bound below; only the budget can end the gather inside it.
        let mut download = paced(200, |_| Duration::from_millis(20));
        let budget = Duration::from_millis(60);
        let (first, second) = run(async {
            compio::time::timeout(Duration::from_secs(2), async {
                let first = chunk(download.gather(200, budget, || true).await);
                let second = chunk(download.gather(200, budget, || true).await);
                (first, second)
            })
            .await
            .expect("the gather waited for the whole limit instead of its budget")
        });
        assert!(first.len() >= 2, "the gather keeps pulling until the budget: {first:?}");
        assert!(first.len() < 200, "the gather ignored its budget: {} bytes", first.len());
        let expected: Vec<u8> = (0..).take(first.len() + second.len()).collect();
        assert_eq!([first, second].concat(), expected, "no frame is lost or reordered");
    }

    #[test]
    fn the_budget_counts_from_the_first_byte_not_from_the_wait_for_it() {
        let budget = Duration::from_millis(50);
        let mut download = paced(16, |n| if n == 0 { Duration::from_millis(150) } else { Duration::ZERO });
        let gathered = run(download.gather(16, budget, || true));
        assert_eq!(chunk(gathered), (0..16).collect::<Vec<u8>>());
    }

    #[test]
    fn closing_the_handle_stops_pulling_at_the_next_frame() {
        let (mut download, pulls) = scripted(&["x"; 64], None, 64);
        run(async {
            // The handle closes while the third frame is in flight.
            closed(&download.gather(64, AMPLE, || pulls.get() < 3).await);
        });
        assert_eq!(pulls.get(), 3, "no frame is pulled after the close is seen");
    }

    #[test]
    fn a_closed_handle_ends_as_closed_from_the_carry_an_end_or_a_failure() {
        // The carry alone fills the chunk, so no frame is pulled.
        let (mut download, pulls) = scripted(&["0123456789"], None, 10);
        run(async {
            assert_eq!(chunk(download.gather(4, AMPLE, || true).await), b"0123");
            closed(&download.gather(4, AMPLE, || false).await);
        });
        assert_eq!(pulls.get(), 1, "the closed gather pulled nothing");

        // The carry, then the object's end.
        let (mut download, _) = scripted(&["012345"], None, 6);
        run(async {
            assert_eq!(chunk(download.gather(4, AMPLE, || true).await), b"0123");
            closed(&download.gather(4, AMPLE, || false).await);
        });

        // The carry, then a backend failure.
        let failure = StorageError::Stream("storage: s3 get body: stalled".into());
        let (mut download, _) = scripted(&["012345"], Some(failure), 6);
        run(async {
            assert_eq!(chunk(download.gather(4, AMPLE, || true).await), b"0123");
            closed(&download.gather(4, AMPLE, || false).await);
        });
    }
}
