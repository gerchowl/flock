//! Test-only barriers over the production I/O completion channel.
use super::*;

impl Transport {
    /// Wait for cancelled I/O to finish its permission-book teardown before a
    /// late-admission assertion. Preserve the real completion values and order
    /// for the collector, including their cancellation errors. The existing
    /// ten-second worker-test ceiling fails a missing or silent producer.
    pub(in crate::structured) fn settle_cancelled_workers(&mut self) {
        assert!(self.input.cancel.load(Ordering::Acquire));
        let deadline = Instant::now() + Duration::from_secs(10);
        let (sender, receiver) = mpsc::channel();
        let mut seen = [false; 2];
        for _ in 0..2 {
            let completion = self
                .completion
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("cancelled I/O workers must complete within the worker-test ceiling");
            let (index, result) = match &completion {
                Completion::Reader(result) => (0, result),
                Completion::Writer(result) => (1, result),
            };
            assert!(!seen[index], "duplicate I/O worker completion");
            seen[index] = true;
            assert_eq!(
                result.as_ref().unwrap_err().kind(),
                io::ErrorKind::Interrupted,
                "fixture requires cancelled I/O, not an unrelated failure"
            );
            sender.send(completion).unwrap();
        }
        self.completion = receiver;
    }
}
