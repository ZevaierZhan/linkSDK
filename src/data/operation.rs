use super::{DataError, DataErrorKind, DataResult};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
/// One absolute deadline shared by all attempts. Clone to cancel in another thread.
#[derive(Clone)]
pub struct Operation {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    max_attempts: usize,
}
impl Operation {
    pub fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            cancelled: Arc::new(AtomicBool::new(false)),
            max_attempts: 3,
        }
    }
    pub fn with_max_attempts(mut self, attempts: usize) -> Self {
        self.max_attempts = attempts.clamp(1, 128);
        self
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    pub(crate) fn for_timeout(&self, timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            cancelled: self.cancelled.clone(),
            max_attempts: self.max_attempts,
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    pub fn check(&self) -> DataResult<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            Err(DataError::new(
                DataErrorKind::Cancelled,
                "data operation cancelled",
            ))
        } else if Instant::now() >= self.deadline {
            Err(DataError::new(
                DataErrorKind::DeadlineExceeded,
                "data operation deadline exceeded",
            ))
        } else {
            Ok(())
        }
    }
    pub(crate) fn retry<T>(&self, mut call: impl FnMut() -> DataResult<T>) -> DataResult<T> {
        for attempt in 0..self.max_attempts {
            self.check()?;
            match call() {
                Err(e) if e.is_retryable() && attempt + 1 < self.max_attempts => {
                    let until =
                        Instant::now() + Duration::from_millis((5u64 << attempt.min(3)).min(40));
                    while Instant::now() < until {
                        self.check()?;
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                result => return result,
            }
        }
        unreachable!()
    }
    pub(crate) fn configure(&self, db: &rusqlite::Connection) -> DataResult<()> {
        db.busy_timeout(Duration::ZERO)
            .map_err(|e| DataError::sqlite(&e))?;
        let op = self.clone();
        db.progress_handler(1000, Some(move || op.check().is_err()))
            .map_err(|e| DataError::sqlite(&e))
    }
    pub(crate) fn translate<T>(&self, result: DataResult<T>) -> DataResult<T> {
        // Successful COMMIT is acknowledged even if cancellation races after it.
        if result.is_err() {
            self.check()?;
        }
        result
    }
}
impl Default for Operation {
    fn default() -> Self {
        Self::new(Duration::from_secs(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_interrupts_sql_work_without_a_busy_wait() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        let op = Operation::new(Duration::from_secs(2));
        op.configure(&db).unwrap();
        let cancel = op.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            cancel.cancel();
        });
        let result=db.query_row("WITH RECURSIVE x(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM x WHERE n<1000000000) SELECT sum(n) FROM x",[],|r|r.get::<_,i64>(0)).map_err(|e|DataError::sqlite(&e));
        assert_eq!(
            op.translate(result).unwrap_err().kind,
            DataErrorKind::Cancelled
        );
        worker.join().unwrap();
    }
}
